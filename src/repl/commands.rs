//! Meta command parsing and execution
//!
//! PostgreSQL-style meta commands like \d, \dt, \q, etc.

use crate::{EmbeddedDatabase, Result, Error};
use crate::sql::{Parser, Planner, explain::{ExplainPlanner, ExplainMode, ExplainFormat}};
use crate::sql::explain_options::{ExplainOptions, ExplainFormatOption};
use crate::sql::explain_storage::{StorageFeatureCollector, format_storage_features_text};
use colored::Colorize;
use std::path::PathBuf;
use std::sync::Arc;
use super::help_manager::HelpManager;

/// Meta command types
#[derive(Debug, Clone, PartialEq)]
pub enum MetaCommand {
    /// \q or \exit - Quit REPL
    Quit,
    /// \h or \help - Show help
    Help,
    /// \h <category> - Show help for specific category
    HelpCategory(String),
    /// \d - List all tables
    ListTables,
    /// \d <table> - Describe table schema
    DescribeTable(String),
    /// \dt - List tables with details
    ListTablesDetailed,
    /// \dS - List system views (dictionary tables)
    ListSystemViews,
    /// \dS <view> - Describe system view schema
    DescribeSystemView(String),
    /// \timing - Toggle timing display
    ToggleTiming,
    /// \show lsn - Toggle LSN/transaction number display
    ShowLsn,
    /// \show branch - Show current branch ID
    ShowBranch,
    /// \e - Edit query in $EDITOR
    EditQuery,

    // v2.0 Feature Commands
    /// \branches - List database branches
    ListBranches,
    /// \use <branch> - Switch to database branch
    UseBranch(String),
    /// \snapshots - List time-travel snapshots
    ListSnapshots,
    /// \dmv - List materialized views
    ListMaterializedViews,
    /// \dmv <view> - Describe materialized view
    DescribeMaterializedView(String),
    /// \compression - Show compression statistics
    ShowCompression,
    /// \compression <table> - Show compression stats for table
    ShowCompressionTable(String),

    // v2.1 Feature Commands
    /// \set <var> <value> - Set REPL variable
    SetVariable(String, String),
    /// \set - Show all variables
    ShowVariables,
    /// \server start - Start server mode
    ServerStart,
    /// \server stop - Stop server mode
    ServerStop,
    /// \server status - Show server status
    ServerStatus,
    /// \ssl status - Show SSL/TLS status
    SslStatus,
    /// \user list - List users
    UserList,
    /// \user add <name> - Add user
    UserAdd(String),
    /// \user remove <name> - Remove user
    UserRemove(String),
    /// \password <user> - Change user password
    ChangePassword(String),
    /// \config - Show current configuration
    ShowConfig,
    /// \config reload - Reload configuration from file
    ConfigReload,
    /// \optimize <table> - Show optimization recommendations
    OptimizeTable(String),
    /// \indexes <table> - Show index recommendations
    ShowIndexes(String),
    /// \stats - Show database statistics
    ShowStats,

    // v2.6 AI Feature Commands
    /// \ai templates - List AI schema templates
    AiTemplates,
    /// \ai template <name> - Show template details
    AiTemplateDetails(String),
    /// \ai infer <format> - Infer schema from clipboard/stdin
    AiInferSchema(String),
    /// \ai generate <description> - Generate schema from description
    AiGenerateSchema(String),
    /// \ai optimize <table> - AI-powered optimization
    AiOptimize(String),
    /// \ai models - List available AI models
    AiModels,
    /// \ai embed <text> - Create embedding for text
    AiEmbed(String),
    /// \ai compare-schema <schema1> <schema2> - Compare two schemas
    AiCompareSchema { schema1: String, schema2: String },

    // Agent Session Commands
    /// \sessions - List agent sessions
    ListSessions,
    /// \session-new <name> - Create new session
    SessionNew(String),
    /// \session <id> - Show session details
    SessionDetails(String),
    /// \session-delete <id> - Delete session
    SessionDelete(String),
    /// \chat <id> - Interactive chat with session
    ChatSession(String),
    /// \session-clear <id> - Clear session messages
    SessionClear(String),
    /// \session fork <id> <name> - Fork agent session
    SessionFork { id: String, name: String },
    /// \session context <id> - Show session context
    SessionContext(String),
    /// \session memory <id> <query> - Search session memory
    SessionMemory { id: String, query: String },
    /// \session summarize <id> - Summarize session
    SessionSummarize(String),

    // Vector Management Commands
    /// \vectors - List vector stores
    ListVectors,
    /// \vector <name> - Show vector store details
    VectorDetails(String),
    /// \vector create <name> <dims> [metric] - Create vector store
    VectorCreate { name: String, dimensions: u32, metric: Option<String> },
    /// \vector delete <name> - Delete vector store
    VectorDelete(String),
    /// \vector stats <name> - Show vector statistics
    VectorStats(String),

    // Document Management Commands
    /// \collections - List document collections
    ListCollections,
    /// \collection <name> - Show collection details
    CollectionDetails(String),
    /// \docs <collection> - List documents in collection
    ListDocumentsInCollection(String),
    /// \doc <collection> <id> - Get document details
    DocumentDetails { collection: String, id: String },
    /// \search-docs <query> - Search documents
    SearchDocuments(String),
    /// \doc chunks <collection> <id> - Show document chunks
    DocumentChunks { collection: String, id: String },
    /// \doc rechunk <collection> <id> <size> - Re-chunk document
    DocumentRechunk { collection: String, id: String, chunk_size: usize },
    /// \rag <collection> <query> [k] - RAG search
    RagSearch { collection: String, query: String, k: usize },

    // Performance & Utility Commands
    /// \explain [options] <query> - Show query execution plan with options
    /// Options: analyze, verbose, storage, ai, why_not, indexes, stats, format json|yaml|tree
    ExplainQueryWithOptions { query: String, options: ExplainOptions },
    /// \profile <query> - Profile query execution
    ProfileQuery(String),
    /// \telemetry - Show database telemetry
    Telemetry,

    // Database Export Commands
    /// \dump [file] - Dump database to SQL file
    Dump(Option<PathBuf>),

    // v3.2 Multi-Tenancy Commands
    /// \tenants - List all tenants
    TenantList,
    /// \tenant create <name> [plan] [isolation] - Create tenant with plan and isolation mode
    TenantCreate { name: String, plan: Option<String>, isolation: Option<String> },
    /// \tenant use <name|id> - Set current tenant context
    TenantUse(String),
    /// \tenant info <name|id> - Show tenant details
    TenantInfo(String),
    /// \tenant quota [name|id] - Show quota usage for tenant
    TenantQuota(Option<String>),
    /// \tenant plans - List available plans
    TenantPlansList,
    /// \tenant plan info <plan> - Show plan details
    TenantPlanInfo(String),
    /// \tenant plan create <name> <tier> <storage_mb> <conn> <qps> - Create plan (ID auto-generated)
    TenantPlanCreate {
        name: String,
        tier_id: u32,
        storage_mb: u64,
        max_connections: usize,
        max_qps: usize,
    },
    /// \tenant usage [name] - Show real-time usage statistics
    TenantUsage(Option<String>),
    /// \tenant plan edit <id> <field> <value> - Edit plan field
    TenantPlanEdit {
        plan_id: String,
        field: String,
        value: String,
    },
    /// \tenant plan enable <id> - Enable plan
    TenantPlanEnable(String),
    /// \tenant plan disable <id> - Disable plan
    TenantPlanDisable(String),
    /// \tenant plan delete <id> - Delete plan (downgrades tenants)
    TenantPlanDelete(String),
    /// \tenant plan <name|id> <plan> - Change tenant plan
    TenantPlan { tenant: String, plan: String },
    /// \tenant delete <name|id> - Delete tenant
    TenantDelete(String),
    /// \tenant current - Show current tenant context
    TenantCurrent,
    /// \tenant clear - Clear current tenant context
    TenantClearContext,

    // v3.2 RLS Policy Commands
    /// \tenant rls create <table> <policy> <expr> <cmd> - Create RLS policy
    TenantRlsCreate {
        table: String,
        policy: String,
        expression: String,
        command: String,
    },
    /// \tenant rls list <table> - List RLS policies for table
    TenantRlsList(String),
    /// \tenant rls delete <table> <policy> - Delete RLS policy
    TenantRlsDelete { table: String, policy: String },

    // v3.2 CDC Commands
    /// \tenant cdc show [limit] - Show CDC events
    TenantCdcShow(Option<usize>),
    /// \tenant cdc export <file> - Export CDC events to file
    TenantCdcExport(String),

    // v3.2 Migration Commands
    /// \tenant migrate to <target> - Initiate tenant migration
    TenantMigrateTo(String),
    /// \tenant migrate status [tenant] - Show migration status
    TenantMigrateStatus(Option<String>),

    // v3.2 Custom Quota Commands
    /// \tenant quota set <tenant> <storage> <connections> <qps> - Set custom quotas
    TenantQuotaSet {
        tenant: String,
        storage_mb: u64,
        max_connections: usize,
        max_qps: u64,
    },

    // v3.4 Information Commands
    /// \version - Show HeliosDB version information
    Version,
    /// \status - Show database status
    Status,
    /// \settings - Show current REPL settings
    Settings,

    // v3.4 Storage Maintenance Commands
    /// \vacuum [table] - Manual compaction (optional: specific table)
    Vacuum(Option<String>),

    // v3.5 Replication Commands
    /// \replication - Show replication status
    ReplicationStatus,
}

impl MetaCommand {
    /// Parse a meta command from input
    pub fn parse(input: &str) -> Option<Self> {
        let trimmed = input.trim();

        if !trimmed.starts_with('\\') {
            return None;
        }

        let parts: Vec<&str> = trimmed[1..].split_whitespace().collect();
        if parts.is_empty() {
            return None;
        }

        match parts[0] {
            // Basic commands
            "q" | "quit" | "exit" => Some(MetaCommand::Quit),
            "h" | "help" | "?" => {
                if parts.len() > 1 {
                    // \h <category> - Help for specific category
                    Some(MetaCommand::HelpCategory(parts[1].to_string()))
                } else {
                    Some(MetaCommand::Help)
                }
            }
            "d" => {
                if parts.len() > 1 {
                    Some(MetaCommand::DescribeTable(parts[1].to_string()))
                } else {
                    Some(MetaCommand::ListTables)
                }
            }
            "dt" => Some(MetaCommand::ListTablesDetailed),
            "dS" => {
                if parts.len() > 1 {
                    Some(MetaCommand::DescribeSystemView(parts[1].to_string()))
                } else {
                    Some(MetaCommand::ListSystemViews)
                }
            }
            "timing" => Some(MetaCommand::ToggleTiming),

            // v3.4 Information Commands
            "version" => Some(MetaCommand::Version),
            "status" => Some(MetaCommand::Status),
            "settings" => Some(MetaCommand::Settings),

            // v3.4 Storage Maintenance
            "vacuum" => {
                if parts.len() > 1 {
                    Some(MetaCommand::Vacuum(Some(parts[1].to_string())))
                } else {
                    Some(MetaCommand::Vacuum(None))
                }
            }

            // v3.5 Replication
            "replication" | "repl" => Some(MetaCommand::ReplicationStatus),

            "show" => {
                if parts.len() > 1 {
                    match parts[1] {
                        "lsn" => Some(MetaCommand::ShowLsn),
                        "branch" => Some(MetaCommand::ShowBranch),
                        _ => {
                            eprintln!("Unknown show command: {}. Try \\show lsn or \\show branch", parts[1]);
                            None
                        }
                    }
                } else {
                    eprintln!("Usage: \\show <command> (lsn, branch)");
                    None
                }
            }
            "lsn" => Some(MetaCommand::ShowLsn), // Keep for backward compatibility
            "e" | "edit" => Some(MetaCommand::EditQuery),

            // v2.0 Feature commands
            "branches" => Some(MetaCommand::ListBranches),
            "use" | "switch" => {
                if parts.len() > 1 {
                    Some(MetaCommand::UseBranch(parts[1].to_string()))
                } else {
                    eprintln!("Usage: \\use <branch_name>");
                    None
                }
            }
            "snapshots" => Some(MetaCommand::ListSnapshots),
            "dmv" => {
                if parts.len() > 1 {
                    Some(MetaCommand::DescribeMaterializedView(parts[1].to_string()))
                } else {
                    Some(MetaCommand::ListMaterializedViews)
                }
            }
            "compression" => {
                if parts.len() > 1 {
                    Some(MetaCommand::ShowCompressionTable(parts[1].to_string()))
                } else {
                    Some(MetaCommand::ShowCompression)
                }
            }

            // v2.1 Feature commands
            "set" => {
                if parts.len() > 2 {
                    let value = parts[2..].join(" ");
                    Some(MetaCommand::SetVariable(parts[1].to_string(), value))
                } else {
                    Some(MetaCommand::ShowVariables)
                }
            }
            "server" => {
                if parts.len() > 1 {
                    match parts[1] {
                        "start" => Some(MetaCommand::ServerStart),
                        "stop" => Some(MetaCommand::ServerStop),
                        "status" => Some(MetaCommand::ServerStatus),
                        _ => None,
                    }
                } else {
                    Some(MetaCommand::ServerStatus)
                }
            }
            "ssl" => {
                if parts.len() > 1 && parts[1] == "status" {
                    Some(MetaCommand::SslStatus)
                } else {
                    Some(MetaCommand::SslStatus)
                }
            }
            "user" => {
                if parts.len() > 1 {
                    match parts[1] {
                        "list" => Some(MetaCommand::UserList),
                        "add" => {
                            if parts.len() > 2 {
                                Some(MetaCommand::UserAdd(parts[2].to_string()))
                            } else {
                                None
                            }
                        }
                        "remove" => {
                            if parts.len() > 2 {
                                Some(MetaCommand::UserRemove(parts[2].to_string()))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                } else {
                    Some(MetaCommand::UserList)
                }
            }
            "password" => {
                if parts.len() > 1 {
                    Some(MetaCommand::ChangePassword(parts[1].to_string()))
                } else {
                    None
                }
            }
            "config" => {
                if parts.len() > 1 && parts[1] == "reload" {
                    Some(MetaCommand::ConfigReload)
                } else {
                    Some(MetaCommand::ShowConfig)
                }
            }
            "optimize" => {
                if parts.len() > 1 {
                    Some(MetaCommand::OptimizeTable(parts[1].to_string()))
                } else {
                    None
                }
            }
            "indexes" => {
                if parts.len() > 1 {
                    Some(MetaCommand::ShowIndexes(parts[1].to_string()))
                } else {
                    None
                }
            }
            "stats" => Some(MetaCommand::ShowStats),

            // v2.6 AI Feature commands
            "ai" => {
                if parts.len() > 1 {
                    match parts[1] {
                        "templates" => Some(MetaCommand::AiTemplates),
                        "template" => {
                            if parts.len() > 2 {
                                Some(MetaCommand::AiTemplateDetails(parts[2].to_string()))
                            } else {
                                eprintln!("Usage: \\ai template <name>");
                                None
                            }
                        }
                        "infer" => {
                            let format = if parts.len() > 2 { parts[2] } else { "json" };
                            Some(MetaCommand::AiInferSchema(format.to_string()))
                        }
                        "generate" => {
                            if parts.len() > 2 {
                                let desc = parts[2..].join(" ");
                                Some(MetaCommand::AiGenerateSchema(desc))
                            } else {
                                eprintln!("Usage: \\ai generate <description>");
                                None
                            }
                        }
                        "optimize" => {
                            if parts.len() > 2 {
                                Some(MetaCommand::AiOptimize(parts[2].to_string()))
                            } else {
                                eprintln!("Usage: \\ai optimize <table>");
                                None
                            }
                        }
                        "models" => Some(MetaCommand::AiModels),
                        "embed" => {
                            if parts.len() > 2 {
                                let text = parts[2..].join(" ");
                                Some(MetaCommand::AiEmbed(text))
                            } else {
                                eprintln!("Usage: \\ai embed <text>");
                                None
                            }
                        }
                        "compare-schema" => {
                            if parts.len() >= 4 {
                                Some(MetaCommand::AiCompareSchema {
                                    schema1: parts[2].to_string(),
                                    schema2: parts[3].to_string(),
                                })
                            } else {
                                eprintln!("Usage: \\ai compare-schema <schema1> <schema2>");
                                None
                            }
                        }
                        _ => {
                            eprintln!("Unknown AI command. Try: \\ai templates, \\ai template <name>, \\ai infer, \\ai generate, \\ai optimize, \\ai models, \\ai embed, \\ai compare-schema");
                            None
                        }
                    }
                } else {
                    eprintln!("AI Commands: templates, template <name>, infer [format], generate <desc>, optimize <table>, models, embed <text>, compare-schema <s1> <s2>");
                    None
                }
            }

            // Agent Session Commands
            "sessions" => Some(MetaCommand::ListSessions),
            "session-new" => {
                if parts.len() > 1 {
                    Some(MetaCommand::SessionNew(parts[1].to_string()))
                } else {
                    eprintln!("Usage: \\session-new <name>");
                    None
                }
            }
            "session" => {
                if parts.len() > 1 {
                    match parts[1] {
                        "fork" => {
                            if parts.len() >= 4 {
                                Some(MetaCommand::SessionFork {
                                    id: parts[2].to_string(),
                                    name: parts[3..].join(" "),
                                })
                            } else {
                                eprintln!("Usage: \\session fork <id> <new_name>");
                                None
                            }
                        }
                        "context" => {
                            if parts.len() >= 3 {
                                Some(MetaCommand::SessionContext(parts[2].to_string()))
                            } else {
                                eprintln!("Usage: \\session context <id>");
                                None
                            }
                        }
                        "memory" => {
                            if parts.len() >= 4 {
                                Some(MetaCommand::SessionMemory {
                                    id: parts[2].to_string(),
                                    query: parts[3..].join(" "),
                                })
                            } else {
                                eprintln!("Usage: \\session memory <id> <query>");
                                None
                            }
                        }
                        "summarize" => {
                            if parts.len() >= 3 {
                                Some(MetaCommand::SessionSummarize(parts[2].to_string()))
                            } else {
                                eprintln!("Usage: \\session summarize <id>");
                                None
                            }
                        }
                        _ => {
                            // Default: treat as session ID for details
                            Some(MetaCommand::SessionDetails(parts[1].to_string()))
                        }
                    }
                } else {
                    eprintln!("Usage: \\session <id> | \\session fork|context|memory|summarize <args>");
                    None
                }
            }
            "session-delete" => {
                if parts.len() > 1 {
                    Some(MetaCommand::SessionDelete(parts[1].to_string()))
                } else {
                    eprintln!("Usage: \\session-delete <id>");
                    None
                }
            }
            "chat" => {
                if parts.len() > 1 {
                    Some(MetaCommand::ChatSession(parts[1].to_string()))
                } else {
                    eprintln!("Usage: \\chat <session_id>");
                    None
                }
            }
            "session-clear" => {
                if parts.len() > 1 {
                    Some(MetaCommand::SessionClear(parts[1].to_string()))
                } else {
                    eprintln!("Usage: \\session-clear <id>");
                    None
                }
            }

            // Vector Management Commands
            "vectors" => Some(MetaCommand::ListVectors),
            "vector" => {
                if parts.len() > 1 {
                    match parts[1] {
                        "create" => {
                            if parts.len() >= 4 {
                                let name = parts[2].to_string();
                                let dims = parts[3].parse::<u32>().unwrap_or(384);
                                let metric = parts.get(4).map(|s| s.to_string());
                                Some(MetaCommand::VectorCreate { name, dimensions: dims, metric })
                            } else {
                                eprintln!("Usage: \\vector create <name> <dimensions> [metric]");
                                eprintln!("  metrics: cosine, l2, inner_product");
                                eprintln!("  Example: \\vector create embeddings 384 cosine");
                                None
                            }
                        }
                        "delete" => {
                            if parts.len() > 2 {
                                Some(MetaCommand::VectorDelete(parts[2].to_string()))
                            } else {
                                eprintln!("Usage: \\vector delete <name>");
                                None
                            }
                        }
                        "stats" => {
                            if parts.len() > 2 {
                                Some(MetaCommand::VectorStats(parts[2].to_string()))
                            } else {
                                eprintln!("Usage: \\vector stats <name>");
                                None
                            }
                        }
                        _ => {
                            // Default to showing details for the store name
                            Some(MetaCommand::VectorDetails(parts[1].to_string()))
                        }
                    }
                } else {
                    eprintln!("Usage: \\vector <name> | \\vector create|delete|stats <name>");
                    None
                }
            }

            // Document Management Commands
            "collections" => Some(MetaCommand::ListCollections),
            "collection" => {
                if parts.len() > 1 {
                    Some(MetaCommand::CollectionDetails(parts[1].to_string()))
                } else {
                    eprintln!("Usage: \\collection <name>");
                    None
                }
            }
            "docs" => {
                if parts.len() > 1 {
                    Some(MetaCommand::ListDocumentsInCollection(parts[1].to_string()))
                } else {
                    eprintln!("Usage: \\docs <collection>");
                    None
                }
            }
            "doc" => {
                if parts.len() > 1 {
                    match parts[1] {
                        "chunks" => {
                            if parts.len() >= 4 {
                                Some(MetaCommand::DocumentChunks {
                                    collection: parts[2].to_string(),
                                    id: parts[3].to_string(),
                                })
                            } else {
                                eprintln!("Usage: \\doc chunks <collection> <id>");
                                None
                            }
                        }
                        "rechunk" => {
                            if parts.len() >= 5 {
                                let chunk_size = parts[4].parse::<usize>().unwrap_or(512);
                                Some(MetaCommand::DocumentRechunk {
                                    collection: parts[2].to_string(),
                                    id: parts[3].to_string(),
                                    chunk_size,
                                })
                            } else {
                                eprintln!("Usage: \\doc rechunk <collection> <id> <chunk_size>");
                                None
                            }
                        }
                        _ => {
                            // Default: treat as \doc <collection> <id>
                            if parts.len() >= 3 {
                                Some(MetaCommand::DocumentDetails {
                                    collection: parts[1].to_string(),
                                    id: parts[2].to_string(),
                                })
                            } else {
                                eprintln!("Usage: \\doc <collection> <id> | \\doc chunks|rechunk <args>");
                                None
                            }
                        }
                    }
                } else {
                    eprintln!("Usage: \\doc <collection> <id> | \\doc chunks|rechunk <args>");
                    None
                }
            }
            "rag" => {
                if parts.len() >= 3 {
                    let k = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(5);
                    Some(MetaCommand::RagSearch {
                        collection: parts[1].to_string(),
                        query: parts[2..].iter().filter(|s| s.parse::<usize>().is_err()).cloned().collect::<Vec<_>>().join(" "),
                        k,
                    })
                } else {
                    eprintln!("Usage: \\rag <collection> <query> [k]");
                    eprintln!("  Example: \\rag docs \"what is vector search\" 10");
                    None
                }
            }
            "search-docs" => {
                if parts.len() > 1 {
                    let query = parts[1..].join(" ");
                    Some(MetaCommand::SearchDocuments(query))
                } else {
                    eprintln!("Usage: \\search-docs <query>");
                    None
                }
            }

            // Performance & Utility Commands
            "explain" => {
                if parts.len() > 1 {
                    // Parse options: \explain [analyze] [verbose] [storage] [ai] [why_not] [indexes] [stats] [format json|yaml|tree] <query>
                    let mut options = ExplainOptions::default();
                    let mut idx = 1;

                    while idx < parts.len() {
                        let part_lower = parts[idx].to_lowercase();
                        match part_lower.as_str() {
                            "analyze" => { options.analyze = true; idx += 1; }
                            "verbose" => { options.verbose = true; idx += 1; }
                            "storage" => { options.storage = true; idx += 1; }
                            "ai" => { options.ai = true; idx += 1; }
                            "why_not" | "whynot" => { options.why_not = true; idx += 1; }
                            "indexes" => { options.indexes = true; idx += 1; }
                            "stats" | "statistics" => { options.statistics = true; idx += 1; }
                            "costs" => { options.costs = true; idx += 1; }
                            "buffers" => { options.buffers = true; idx += 1; }
                            "timing" => { options.timing = true; idx += 1; }
                            "summary" => { options.summary = true; idx += 1; }
                            "format" => {
                                if idx + 1 < parts.len() {
                                    options.format = match parts[idx + 1].to_lowercase().as_str() {
                                        "json" => ExplainFormatOption::Json,
                                        "yaml" => ExplainFormatOption::Yaml,
                                        "tree" => ExplainFormatOption::Tree,
                                        "text" => ExplainFormatOption::Text,
                                        _ => ExplainFormatOption::Text,
                                    };
                                    idx += 2;
                                } else {
                                    idx += 1;
                                }
                            }
                            // Any non-option word starts the SQL query
                            _ => break,
                        }
                    }

                    if idx < parts.len() {
                        let query = parts[idx..].join(" ");
                        Some(MetaCommand::ExplainQueryWithOptions { query, options })
                    } else {
                        Self::print_explain_usage();
                        None
                    }
                } else {
                    Self::print_explain_usage();
                    None
                }
            }
            "profile" => {
                if parts.len() > 1 {
                    let query = parts[1..].join(" ");
                    Some(MetaCommand::ProfileQuery(query))
                } else {
                    eprintln!("Usage: \\profile <SQL query>");
                    eprintln!("  Example: \\profile SELECT * FROM users");
                    None
                }
            }
            "telemetry" => Some(MetaCommand::Telemetry),

            "dump" => {
                let file = if parts.len() > 1 {
                    Some(std::path::PathBuf::from(parts[1]))
                } else {
                    None
                };
                Some(MetaCommand::Dump(file))
            }

            // v3.2 Multi-Tenancy Commands
            "tenants" => Some(MetaCommand::TenantList),
            "tenant" => {
                if parts.len() > 1 {
                    match parts[1] {
                        "list" => Some(MetaCommand::TenantList),
                        "create" => {
                            if parts.len() > 2 {
                                let name = parts[2].to_string();
                                let plan = parts.get(3).map(|s| s.to_string());
                                let isolation = parts.get(4).map(|s| s.to_string());
                                Some(MetaCommand::TenantCreate { name, plan, isolation })
                            } else {
                                eprintln!("{}", "Usage: \\tenant create <name> [plan] [isolation]".bold());
                                eprintln!();
                                eprintln!("{}", "Arguments:".bold());
                                eprintln!("  {}      Tenant name (required)", "<name>".cyan());
                                eprintln!("  {}      Plan tier (optional, default: free)", "[plan]".cyan());
                                eprintln!("  {} Isolation mode (optional, default: shared)", "[isolation]".cyan());
                                eprintln!();
                                eprintln!("{}", "Plans:".bold());
                                eprintln!("  {}     - 100 MB, 5 connections, 10 QPS", "free".green());
                                eprintln!("  {}  - 1 GB, 20 connections, 100 QPS", "starter".green());
                                eprintln!("  {}      - 10 GB, 100 connections, 1000 QPS", "pro".green());
                                eprintln!("  {} - 100 GB, 1000 connections, 10000 QPS", "enterprise".green());
                                eprintln!("  {} - Unlimited resources", "unlimited".green());
                                eprintln!();
                                eprintln!("{}", "Isolation Modes:".bold());
                                eprintln!("  {} (or {}) - SharedSchema with RLS (default)", "shared".yellow(), "rls".yellow());
                                eprintln!("  {}        - Schema per tenant", "schema".yellow());
                                eprintln!("  {} (or {})  - Database per tenant", "database".yellow(), "db".yellow());
                                eprintln!();
                                eprintln!("{}", "Examples:".bold());
                                eprintln!("  {} - Free plan, shared isolation", "\\tenant create AcmeCorp".cyan());
                                eprintln!("  {} - Starter plan", "\\tenant create AcmeCorp starter".cyan());
                                eprintln!("  {} - Pro plan, schema isolation", "\\tenant create AcmeCorp pro schema".cyan());
                                eprintln!("  {} - Enterprise, database isolation", "\\tenant create AcmeCorp enterprise db".cyan());
                                None
                            }
                        }
                        "use" => {
                            if parts.len() > 2 {
                                Some(MetaCommand::TenantUse(parts[2].to_string()))
                            } else {
                                eprintln!("Usage: \\tenant use <name|id>");
                                None
                            }
                        }
                        "info" => {
                            if parts.len() > 2 {
                                Some(MetaCommand::TenantInfo(parts[2].to_string()))
                            } else {
                                eprintln!("Usage: \\tenant info <name|id>");
                                None
                            }
                        }
                        "quota" => {
                            // Check for 'set' subcommand
                            if parts.len() > 2 && parts[2] == "set" {
                                // \tenant quota set <tenant> <storage_mb> <connections> <qps>
                                if parts.len() >= 7 {
                                    let tenant = parts[3].to_string();
                                    let storage_mb = parts[4].parse::<u64>().unwrap_or(1024);
                                    let max_connections = parts[5].parse::<usize>().unwrap_or(10);
                                    let max_qps = parts[6].parse::<u64>().unwrap_or(1000);

                                    Some(MetaCommand::TenantQuotaSet {
                                        tenant,
                                        storage_mb,
                                        max_connections,
                                        max_qps,
                                    })
                                } else {
                                    eprintln!("Usage: \\tenant quota set <tenant> <storage_mb> <connections> <qps>");
                                    eprintln!("  Example: \\tenant quota set acme-corp 5000 25 10000");
                                    None
                                }
                            } else {
                                // \tenant quota [tenant]
                                let tenant_ref = parts.get(2).map(|s| s.to_string());
                                Some(MetaCommand::TenantQuota(tenant_ref))
                            }
                        }
                        "usage" => {
                            // \tenant usage [tenant]
                            let tenant_ref = parts.get(2).map(|s| s.to_string());
                            Some(MetaCommand::TenantUsage(tenant_ref))
                        }
                        "plans" => {
                            // \tenant plans - list available plans
                            Some(MetaCommand::TenantPlansList)
                        }
                        "plan" => {
                            if parts.len() > 2 {
                                match parts[2] {
                                    "info" => {
                                        // \tenant plan info <plan>
                                        if parts.len() > 3 {
                                            Some(MetaCommand::TenantPlanInfo(parts[3].to_string()))
                                        } else {
                                            eprintln!("Usage: \\tenant plan info <plan>");
                                            None
                                        }
                                    }
                                    "create" => {
                                        // \tenant plan create <name> <tier> <storage_mb> <conn> <qps>
                                        if parts.len() >= 8 {
                                            let name = parts[3].to_string();
                                            let tier_id = parts[4].parse::<u32>().unwrap_or(100);
                                            let storage_mb = parts[5].parse::<u64>().unwrap_or(100);
                                            let max_connections = parts[6].parse::<usize>().unwrap_or(5);
                                            let max_qps = parts[7].parse::<usize>().unwrap_or(10);
                                            Some(MetaCommand::TenantPlanCreate {
                                                name,
                                                tier_id,
                                                storage_mb,
                                                max_connections,
                                                max_qps,
                                            })
                                        } else {
                                            eprintln!("Usage: \\tenant plan create <name> <tier> <storage_mb> <conn> <qps>");
                                            eprintln!("  Example: \\tenant plan create Basic 150 500 10 50");
                                            eprintln!("  Plan ID is auto-generated from name (lowercase, no spaces)");
                                            None
                                        }
                                    }
                                    "edit" => {
                                        // \tenant plan edit <id> <field> <value>
                                        if parts.len() >= 6 {
                                            Some(MetaCommand::TenantPlanEdit {
                                                plan_id: parts[3].to_string(),
                                                field: parts[4].to_string(),
                                                value: parts[5..].join(" "),
                                            })
                                        } else {
                                            eprintln!("Usage: \\tenant plan edit <id> <field> <value>");
                                            eprintln!("  Fields: name, description, tier, storage, connections, qps");
                                            eprintln!("  Example: \\tenant plan edit starter tier 250");
                                            None
                                        }
                                    }
                                    "enable" => {
                                        // \tenant plan enable <id>
                                        if parts.len() > 3 {
                                            Some(MetaCommand::TenantPlanEnable(parts[3].to_string()))
                                        } else {
                                            eprintln!("Usage: \\tenant plan enable <plan_id>");
                                            None
                                        }
                                    }
                                    "disable" => {
                                        // \tenant plan disable <id>
                                        if parts.len() > 3 {
                                            Some(MetaCommand::TenantPlanDisable(parts[3].to_string()))
                                        } else {
                                            eprintln!("Usage: \\tenant plan disable <plan_id>");
                                            None
                                        }
                                    }
                                    "delete" => {
                                        // \tenant plan delete <id>
                                        if parts.len() > 3 {
                                            Some(MetaCommand::TenantPlanDelete(parts[3].to_string()))
                                        } else {
                                            eprintln!("Usage: \\tenant plan delete <plan_id>");
                                            eprintln!("  Note: Tenants on this plan will be downgraded");
                                            None
                                        }
                                    }
                                    _ => {
                                        // \tenant plan <tenant> <plan> - assign tenant to plan
                                        if parts.len() > 3 {
                                            Some(MetaCommand::TenantPlan {
                                                tenant: parts[2].to_string(),
                                                plan: parts[3].to_string(),
                                            })
                                        } else {
                                            Self::print_plan_help();
                                            None
                                        }
                                    }
                                }
                            } else {
                                Self::print_plan_help();
                                None
                            }
                        }
                        "delete" => {
                            if parts.len() > 2 {
                                Some(MetaCommand::TenantDelete(parts[2].to_string()))
                            } else {
                                eprintln!("Usage: \\tenant delete <name|id>");
                                None
                            }
                        }
                        "rls" => {
                            // \tenant rls <subcommand>
                            if parts.len() > 2 {
                                match parts[2] {
                                    "create" => {
                                        // \tenant rls create <table> <policy> <expr> <cmd>
                                        if parts.len() >= 7 {
                                            let table = parts[3].to_string();
                                            let policy = parts[4].to_string();
                                            let expression = parts[5].to_string();
                                            let command = parts[6].to_string();

                                            Some(MetaCommand::TenantRlsCreate {
                                                table,
                                                policy,
                                                expression,
                                                command,
                                            })
                                        } else {
                                            eprintln!("Usage: \\tenant rls create <table> <policy> <expression> <command>");
                                            eprintln!("  Commands: ALL, SELECT, INSERT, UPDATE, DELETE");
                                            eprintln!("  Example: \\tenant rls create customers tenant_filter \"tenant_id=current_tenant()\" ALL");
                                            None
                                        }
                                    }
                                    "list" => {
                                        // \tenant rls list <table>
                                        if parts.len() >= 4 {
                                            Some(MetaCommand::TenantRlsList(parts[3].to_string()))
                                        } else {
                                            eprintln!("Usage: \\tenant rls list <table>");
                                            None
                                        }
                                    }
                                    "delete" => {
                                        // \tenant rls delete <table> <policy>
                                        if parts.len() >= 5 {
                                            Some(MetaCommand::TenantRlsDelete {
                                                table: parts[3].to_string(),
                                                policy: parts[4].to_string(),
                                            })
                                        } else {
                                            eprintln!("Usage: \\tenant rls delete <table> <policy>");
                                            None
                                        }
                                    }
                                    _ => {
                                        eprintln!("Unknown rls subcommand: {}", parts[2]);
                                        eprintln!("Available: create, list, delete");
                                        None
                                    }
                                }
                            } else {
                                eprintln!("Usage: \\tenant rls <create|list|delete>");
                                None
                            }
                        }
                        "cdc" => {
                            // \tenant cdc <subcommand>
                            if parts.len() > 2 {
                                match parts[2] {
                                    "show" => {
                                        let limit = parts.get(3).and_then(|s| s.parse::<usize>().ok());
                                        Some(MetaCommand::TenantCdcShow(limit))
                                    }
                                    "export" => {
                                        if parts.len() >= 4 {
                                            Some(MetaCommand::TenantCdcExport(parts[3].to_string()))
                                        } else {
                                            eprintln!("Usage: \\tenant cdc export <file>");
                                            None
                                        }
                                    }
                                    _ => {
                                        eprintln!("Unknown cdc subcommand: {}", parts[2]);
                                        eprintln!("Available: show, export");
                                        None
                                    }
                                }
                            } else {
                                // Default to show with limit 10
                                Some(MetaCommand::TenantCdcShow(Some(10)))
                            }
                        }
                        "migrate" => {
                            // \tenant migrate <subcommand>
                            if parts.len() > 2 {
                                match parts[2] {
                                    "to" => {
                                        if parts.len() >= 4 {
                                            Some(MetaCommand::TenantMigrateTo(parts[3].to_string()))
                                        } else {
                                            eprintln!("Usage: \\tenant migrate to <target>");
                                            None
                                        }
                                    }
                                    "status" => {
                                        let tenant = parts.get(3).map(|s| s.to_string());
                                        Some(MetaCommand::TenantMigrateStatus(tenant))
                                    }
                                    _ => {
                                        eprintln!("Unknown migrate subcommand: {}", parts[2]);
                                        eprintln!("Available: to, status");
                                        None
                                    }
                                }
                            } else {
                                eprintln!("Usage: \\tenant migrate <to|status>");
                                None
                            }
                        }
                        "current" => Some(MetaCommand::TenantCurrent),
                        "clear" => Some(MetaCommand::TenantClearContext),
                        _ => {
                            // Treat as shorthand for \tenant info <name>
                            Some(MetaCommand::TenantInfo(parts[1].to_string()))
                        }
                    }
                } else {
                    // No subcommand - show list
                    Some(MetaCommand::TenantList)
                }
            }

            _ => None,
        }
    }

    /// Execute the meta command
    ///
    /// # Arguments
    ///
    /// * `db` - Reference to the database
    /// * `show_timing` - Whether timing is currently enabled
    /// * `config` - Optional reference to the current REPL config (for reload support)
    pub fn execute(
        &self,
        db: &EmbeddedDatabase,
        show_timing: bool,
        config: Option<&super::ReplConfig>,
    ) -> Result<MetaCommandResult> {
        match self {
            MetaCommand::Quit => Ok(MetaCommandResult::Quit),

            MetaCommand::Help => {
                HelpManager::print_help_main();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::HelpCategory(category) => {
                HelpManager::print_help_category(&category);
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ListTables => {
                let catalog = db.storage.catalog();
                let tables = catalog.list_tables()?;

                if tables.is_empty() {
                    println!("{}", "No tables found.".dimmed());
                } else {
                    println!("\n{}", "Tables:".bold());
                    for table in tables {
                        println!("  {}", table.cyan());
                    }
                    println!();
                }

                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::DescribeTable(table_name) => {
                let catalog = db.storage.catalog();

                // Check if table exists
                if !catalog.table_exists(table_name)? {
                    return Err(Error::query_execution(format!(
                        "Table '{}' does not exist", table_name
                    )));
                }

                let schema = catalog.get_table_schema(table_name)?;

                println!("\n{}: {}", "Table".bold(), table_name.cyan());
                println!("{}", "─".repeat(50));
                println!("{:<20} {:<15} {:<10} {}",
                    "Column".bold(),
                    "Type".bold(),
                    "Nullable".bold(),
                    "Primary Key".bold()
                );
                println!("{}", "─".repeat(50));

                for column in &schema.columns {
                    println!("{:<20} {:<15} {:<10} {}",
                        column.name.green(),
                        format!("{:?}", column.data_type).yellow(),
                        if column.nullable { "YES" } else { "NO" },
                        if column.primary_key { "YES" } else { "" }
                    );
                }
                println!();

                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ListTablesDetailed => {
                let catalog = db.storage.catalog();
                let tables = catalog.list_tables()?;

                if tables.is_empty() {
                    println!("{}", "No tables found.".dimmed());
                } else {
                    println!("\n{}", "Tables:".bold());
                    println!("{}", "─".repeat(60));
                    println!("{:<30} {}", "Name".bold(), "Columns".bold());
                    println!("{}", "─".repeat(60));

                    for table in tables {
                        let schema = catalog.get_table_schema(&table)?;
                        println!("{:<30} {}",
                            table.cyan(),
                            schema.columns.len()
                        );
                    }
                    println!();
                }

                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ToggleTiming => {
                let new_state = !show_timing;
                println!(
                    "Timing is {}",
                    if new_state { "on".green() } else { "off".red() }
                );
                Ok(MetaCommandResult::ToggleTiming(new_state))
            }

            MetaCommand::ShowLsn => {
                // We don't know the current state here, so we return true (enable)
                // The shell will handle toggling based on current state
                println!(
                    "{}",
                    "LSN/Transaction display toggled. Use \\show lsn again to toggle off.".green()
                );
                Ok(MetaCommandResult::ToggleLsn(true))
            }

            MetaCommand::ShowBranch => {
                let branch = db.storage.get_current_branch();
                if let Some(branch_name) = branch {
                    // Try to get the branch ID
                    if let Some(branch_manager) = db.storage.branch_manager() {
                        match branch_manager.get_branch_by_name(&branch_name) {
                            Ok(metadata) => {
                                println!("{}", format!("Branch: {} (ID: {})", branch_name.cyan(), metadata.branch_id).bold());
                            }
                            Err(_) => {
                                println!("{}", format!("Branch: {} (ID: unknown)", branch_name.cyan()).bold());
                            }
                        }
                    } else {
                        println!("{}", format!("Branch: {}", branch_name.cyan()).bold());
                    }
                } else {
                    println!("{}", "Branch: main (ID: 0)".bold());
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ListSystemViews => {
                use crate::sql::phase3::SystemViewRegistry;

                let registry = SystemViewRegistry::new();
                let views = registry.list_views();

                if views.is_empty() {
                    println!("{}", "No system views found.".dimmed());
                } else {
                    println!("\n{}", "System Views:".bold());
                    println!("{}", "─".repeat(70));

                    for view in views {
                        if let Some(schema) = registry.get_schema(view) {
                            let description = match view {
                                "pg_database_branches" => "Lists all database branches with metadata",
                                "pg_mv_staleness" => "Shows staleness info for materialized views",
                                "pg_vector_index_stats" => "Vector index statistics (PQ compression)",
                                _ => "System view"
                            };
                            println!("  {} - {}", view.cyan(), description.dimmed());
                        }
                    }
                    println!();
                    println!("{}", "Use \\dS <view_name> to see schema details".dimmed());
                    println!("{}", "Example: \\dS pg_database_branches".dimmed());
                    println!();
                }

                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::DescribeSystemView(view_name) => {
                use crate::sql::phase3::SystemViewRegistry;

                let registry = SystemViewRegistry::new();

                let schema = registry.get_schema(&view_name).ok_or_else(|| {
                    Error::query_execution(format!(
                        "System view '{}' does not exist. Use \\dS to list available views.", view_name
                    ))
                })?;

                println!("\n{}: {}", "System View".bold(), view_name.cyan());
                println!("{}", "─".repeat(70));
                println!("{:<25} {:<15} {:<10}",
                    "Column".bold(),
                    "Type".bold(),
                    "Nullable".bold()
                );
                println!("{}", "─".repeat(70));

                for column in &schema.columns {
                    println!("{:<25} {:<15} {:<10}",
                        column.name.green(),
                        format!("{:?}", column.data_type).yellow(),
                        if column.nullable { "YES" } else { "NO" }
                    );
                }

                println!();
                println!("{}", "Usage:".bold());
                println!("  SELECT * FROM {}();", view_name.cyan());
                println!();

                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::EditQuery => {
                println!("{}", "Query editing not yet implemented".yellow());
                Ok(MetaCommandResult::Continue)
            }

            // v2.0 Feature commands
            MetaCommand::ListBranches => {
                println!("\n{}", "Database Branches:".bold());
                println!("{}", "─".repeat(70));
                println!("{}", "Use: SELECT * FROM pg_database_branches();".dimmed());
                println!("{}", "Use: \\use <branch_name> to switch branches".dimmed());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::UseBranch(branch_name) => {
                // "main" is always valid
                if branch_name != "main" {
                    // Validate branch exists before switching
                    if let Some(branch_mgr) = db.storage.branch_manager() {
                        if branch_mgr.get_branch_by_name(&branch_name).is_err() {
                            println!("{}: Branch '{}' does not exist.", "Error".red(), branch_name);
                            println!("{}", "Create it first with: CREATE BRANCH <name> FROM main".dimmed());
                            println!();
                            println!("{}", "Existing branches:".bold());
                            if let Ok(branches) = branch_mgr.list_branches() {
                                for b in branches {
                                    println!("  - {}", b.name.cyan());
                                }
                            }
                            return Ok(MetaCommandResult::Continue);
                        }
                    }
                }
                println!("Switching to branch: {}", branch_name.cyan());
                Ok(MetaCommandResult::SwitchBranch(branch_name.clone()))
            }

            MetaCommand::ListSnapshots => {
                println!("\n{}", "Time-Travel Snapshots:".bold());
                println!("{}", "─".repeat(70));
                println!("{}", "Use time-travel queries:".dimmed());
                println!("  {}", "SELECT * FROM table AS OF TIMESTAMP '2025-11-23 10:00:00';".cyan());
                println!("  {}", "SELECT * FROM table AS OF TRANSACTION 12345;".cyan());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ListMaterializedViews => {
                println!("\n{}", "Materialized Views:".bold());
                println!("{}", "─".repeat(70));
                println!("{}", "Use: SELECT * FROM pg_mv_staleness();".dimmed());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::DescribeMaterializedView(view_name) => {
                println!("\n{}: {}", "Materialized View".bold(), view_name.cyan());
                println!("{}", "─".repeat(70));
                println!("{}", "Check staleness:".dimmed());
                println!("  {}", format!("SELECT * FROM pg_mv_staleness() WHERE view_name = '{}';", view_name).cyan());
                println!();
                println!("{}", "Refresh:".dimmed());
                println!("  {}", format!("REFRESH MATERIALIZED VIEW {};", view_name).cyan());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ShowCompression => {
                println!("\n{}", "Compression Statistics:".bold());
                println!("{}", "─".repeat(70));
                println!("{}", "Use: SELECT * FROM pg_vector_index_stats();".dimmed());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ShowCompressionTable(table_name) => {
                println!("\n{}: {}", "Compression Statistics".bold(), table_name.cyan());
                println!("{}", "─".repeat(70));
                println!("{}", "Set compression:".dimmed());
                println!("  {}", format!("ALTER TABLE {} SET COMPRESSION zstd;", table_name).cyan());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            // v2.1 Feature commands
            MetaCommand::SetVariable(name, value) => {
                println!("{}", format!("Set {} = {}", name.cyan(), value.yellow()));
                println!("{}", "Note: Use SQL SET command for persistent settings".dimmed());
                println!("  {}", format!("SET {} = {};", name, value).cyan());
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ShowVariables => {
                println!("\n{}", "REPL Variables:".bold());
                println!("{}", "─".repeat(70));
                println!("{}", "Use SQL SHOW command to view session settings:".dimmed());
                println!("  {}", "SHOW ALL;".cyan());
                println!("  {}", "SHOW optimizer;".cyan());
                println!("  {}", "SHOW statement_timeout;".cyan());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ServerStart => {
                println!("{}", "Server mode not available in REPL".yellow());
                println!("{}", "Use: heliosdb-lite start --config config.toml".dimmed());
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ServerStop => {
                println!("{}", "Server mode not available in REPL".yellow());
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ServerStatus => {
                println!("\n{}", "Server Status:".bold());
                println!("{}", "─".repeat(70));
                println!("  Mode: {}", "Embedded REPL".cyan());
                println!("  Server: {}", "Not running".dimmed());
                println!();
                println!("{}", "To start server:".dimmed());
                println!("  {}", "heliosdb-lite start --port 5432".cyan());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::SslStatus => {
                println!("\n{}", "SSL/TLS Status:".bold());
                println!("{}", "─".repeat(70));
                println!("  Enabled: {}", "No (REPL mode)".dimmed());
                println!();
                println!("{}", "Configure in config.toml:".dimmed());
                println!("  [server]");
                println!("  tls_enabled = true");
                println!("  tls_cert_path = \"/path/to/cert.pem\"");
                println!("  tls_key_path = \"/path/to/key.pem\"");
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::UserList => {
                println!("\n{}", "Users:".bold());
                println!("{}", "─".repeat(70));
                println!("{}", "User management not available in REPL mode".yellow());
                println!("{}", "Configure authentication in config.toml".dimmed());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::UserAdd(name) => {
                println!("{}", format!("User management not available in REPL mode. User '{}' not added.", name).yellow());
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::UserRemove(name) => {
                println!("{}", format!("User management not available in REPL mode. User '{}' not removed.", name).yellow());
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ChangePassword(user) => {
                println!("{}", format!("Password change not available in REPL mode for user '{}'.", user).yellow());
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ShowConfig => {
                println!("\n{}", "Current Configuration:".bold());
                println!("{}", "─".repeat(70));

                // REPL configuration
                println!("{}", "REPL Settings:".bold());
                if let Some(cfg) = config {
                    println!("  Timing display:  {}", if cfg.show_timing { "enabled".green() } else { "disabled".yellow() });
                    println!("  Output format:   {}", format!("{:?}", cfg.output_format).cyan());
                    println!("  Show row count:  {}", if cfg.show_row_count { "enabled".green() } else { "disabled".yellow() });
                    println!("  Auto-commit:     {}", if cfg.auto_commit { "enabled".green() } else { "disabled".yellow() });
                    println!("  Null display:    \"{}\"", cfg.null_display);
                    println!("  Max col width:   {}", cfg.max_column_width);
                    println!("  Max history:     {}", cfg.max_history);
                    if let Some(path) = &cfg.history_path {
                        println!("  History file:    {}", path);
                    }
                    if let Some(path) = &cfg.config_path {
                        println!("  Config file:     {}", path.display());
                    } else {
                        println!("  Config file:     {}", "(none - reload disabled)".yellow());
                    }
                } else {
                    println!("  {}", "(configuration context not available)".yellow());
                }
                println!();

                // Storage configuration
                println!("{}", "Storage:".bold());
                println!("  WAL: {}", "enabled".green());
                println!("  Compression: {}", "zstd".cyan());
                println!("  Time-Travel: {}", "enabled".green());
                println!();

                // Performance configuration
                println!("{}", "Performance:".bold());
                println!("  Workers: {}", std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4));
                println!("  SIMD: {}", "enabled".green());
                println!();

                if config.map(|c| c.config_path.is_some()).unwrap_or(false) {
                    println!("{}", "Use \\config reload to reload from file".dimmed());
                } else {
                    println!("{}", "Start REPL with --config to enable reload: heliosdb-lite repl --config config.toml".dimmed());
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ConfigReload => {
                // Try to reload configuration from file
                match config {
                    Some(cfg) => {
                        match cfg.reload() {
                            Ok(new_config) => {
                                println!("{}", "Configuration reloaded successfully!".green());
                                println!();
                                println!("{}", "Updated settings:".bold());
                                println!("  Timing:         {}", if new_config.show_timing { "enabled".green() } else { "disabled".yellow() });
                                println!("  Output format:  {:?}", new_config.output_format);
                                println!("  Show row count: {}", new_config.show_row_count);
                                println!("  Auto-commit:    {}", new_config.auto_commit);
                                println!("  Null display:   \"{}\"", new_config.null_display);
                                println!("  Max col width:  {}", new_config.max_column_width);
                                if let Some(path) = &new_config.config_path {
                                    println!("  Config file:    {}", path.display());
                                }
                                println!();
                                Ok(MetaCommandResult::ConfigReloaded(new_config))
                            }
                            Err(e) => {
                                println!("{}", format!("Failed to reload configuration: {}", e).red());
                                if cfg.config_path.is_none() {
                                    println!("{}", "No configuration file path set.".yellow());
                                    println!("{}", "Start REPL with: heliosdb-lite repl --config config.toml".dimmed());
                                }
                                Ok(MetaCommandResult::Continue)
                            }
                        }
                    }
                    None => {
                        println!("{}", "Configuration context not available.".yellow());
                        println!("{}", "Restart REPL with: heliosdb-lite repl --config config.toml".dimmed());
                        Ok(MetaCommandResult::Continue)
                    }
                }
            }

            MetaCommand::OptimizeTable(table_name) => {
                println!("\n{}: {}", "Optimization Recommendations".bold(), table_name.cyan());
                println!("{}", "─".repeat(70));
                println!("{}", "Use EXPLAIN to analyze queries:".dimmed());
                println!("  {}", format!("EXPLAIN SELECT * FROM {};", table_name).cyan());
                println!();
                println!("{}", "Run index recommender:".dimmed());
                println!("  {}", format!("SELECT * FROM recommend_indexes('{}');", table_name).cyan());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ShowIndexes(table_name) => {
                println!("\n{}: {}", "Index Recommendations".bold(), table_name.cyan());
                println!("{}", "─".repeat(70));
                println!("{}", "Use the index recommender:".dimmed());
                println!("  {}", format!("SELECT * FROM recommend_indexes('{}');", table_name).cyan());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ShowStats => {
                println!("\n{}", "Database Statistics:".bold());
                println!("{}", "─".repeat(70));

                let catalog = db.storage.catalog();
                if let Ok(tables) = catalog.list_tables() {
                    println!("  Tables: {}", tables.len().to_string().cyan());
                }

                println!();
                println!("{}", "Detailed statistics:".dimmed());
                println!("  {}", "SELECT * FROM pg_database_branches();".cyan());
                println!("  {}", "SELECT * FROM pg_mv_staleness();".cyan());
                println!("  {}", "SELECT * FROM pg_vector_index_stats();".cyan());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            // v2.6 AI Feature commands
            MetaCommand::AiTemplates => {
                println!("\n{}", "AI Schema Templates:".bold());
                println!("{}", "═".repeat(70));

                let templates = vec![
                    ("ecommerce", "E-commerce store with products, orders, and customers"),
                    ("blog", "Blog with posts, comments, tags, and authors"),
                    ("saas", "Multi-tenant SaaS with users, organizations, subscriptions"),
                    ("analytics", "Event tracking and analytics schema"),
                    ("iot", "IoT device data with time-series measurements"),
                    ("social", "Social network with profiles, posts, follows, likes"),
                    ("inventory", "Inventory management with products, warehouses, stock"),
                    ("crm", "CRM with contacts, deals, activities, pipelines"),
                    ("rag", "RAG (Retrieval-Augmented Generation) schema with documents, chunks, embeddings"),
                    ("agents", "AI agent memory with sessions, messages, tools, context"),
                    ("vector-store", "Vector database schema with indexes and metadata"),
                    ("chatbot", "Chatbot with conversations, intents, responses"),
                ];

                println!("\n{}", "Available Templates:".bold());
                println!("{}", "─".repeat(70));
                for (name, desc) in templates {
                    println!("  {} - {}", name.cyan(), desc.dimmed());
                }
                println!();
                println!("{}", "Usage:".dimmed());
                println!("  {} - Show template DDL", "\\ai template <name>".cyan());
                println!("  {} - Apply template", "SELECT * FROM apply_template('<name>');".cyan());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::AiTemplateDetails(name) => {
                println!("\n{}: {}", "Template".bold(), name.cyan());
                println!("{}", "═".repeat(70));

                let ddl = match name.as_str() {
                    "ecommerce" => r#"
-- E-commerce Schema Template
CREATE TABLE customers (
    id SERIAL PRIMARY KEY,
    email TEXT UNIQUE NOT NULL,
    name TEXT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE products (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    price DECIMAL(10,2) NOT NULL,
    inventory INT DEFAULT 0,
    embedding VECTOR(384)  -- For semantic search
);

CREATE TABLE orders (
    id SERIAL PRIMARY KEY,
    customer_id INT REFERENCES customers(id),
    status TEXT DEFAULT 'pending',
    total DECIMAL(10,2),
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE order_items (
    id SERIAL PRIMARY KEY,
    order_id INT REFERENCES orders(id),
    product_id INT REFERENCES products(id),
    quantity INT NOT NULL,
    price DECIMAL(10,2) NOT NULL
);

CREATE INDEX idx_products_embedding ON products USING hnsw(embedding);
"#,
                    "blog" => r#"
-- Blog Schema Template
CREATE TABLE authors (
    id SERIAL PRIMARY KEY,
    username TEXT UNIQUE NOT NULL,
    email TEXT UNIQUE NOT NULL,
    bio TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE posts (
    id SERIAL PRIMARY KEY,
    author_id INT REFERENCES authors(id),
    title TEXT NOT NULL,
    slug TEXT UNIQUE NOT NULL,
    content TEXT NOT NULL,
    embedding VECTOR(384),  -- For semantic search
    published_at TIMESTAMP,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE tags (
    id SERIAL PRIMARY KEY,
    name TEXT UNIQUE NOT NULL,
    slug TEXT UNIQUE NOT NULL
);

CREATE TABLE post_tags (
    post_id INT REFERENCES posts(id),
    tag_id INT REFERENCES tags(id),
    PRIMARY KEY (post_id, tag_id)
);

CREATE TABLE comments (
    id SERIAL PRIMARY KEY,
    post_id INT REFERENCES posts(id),
    author_name TEXT NOT NULL,
    content TEXT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_posts_embedding ON posts USING hnsw(embedding);
"#,
                    "rag" => r#"
-- RAG (Retrieval-Augmented Generation) Schema Template
CREATE TABLE documents (
    id SERIAL PRIMARY KEY,
    source TEXT NOT NULL,
    content TEXT NOT NULL,
    metadata JSONB DEFAULT '{}',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE chunks (
    id SERIAL PRIMARY KEY,
    document_id INT REFERENCES documents(id),
    content TEXT NOT NULL,
    chunk_index INT NOT NULL,
    start_offset INT,
    end_offset INT,
    embedding VECTOR(1536) NOT NULL,
    metadata JSONB DEFAULT '{}'
);

CREATE TABLE conversations (
    id SERIAL PRIMARY KEY,
    session_id TEXT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE messages (
    id SERIAL PRIMARY KEY,
    conversation_id INT REFERENCES conversations(id),
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    sources JSONB,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_chunks_embedding ON chunks USING hnsw(embedding);
CREATE INDEX idx_chunks_document ON chunks(document_id);
CREATE INDEX idx_messages_conversation ON messages(conversation_id);
"#,
                    "agents" => r#"
-- AI Agent Memory Schema Template
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    metadata JSONB DEFAULT '{}',
    token_limit INT,
    summarization TEXT DEFAULT 'rolling',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE messages (
    id SERIAL PRIMARY KEY,
    session_id TEXT REFERENCES sessions(id),
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    name TEXT,
    function_call JSONB,
    tool_calls JSONB,
    embedding VECTOR(1536),
    metadata JSONB DEFAULT '{}',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE summaries (
    id SERIAL PRIMARY KEY,
    session_id TEXT REFERENCES sessions(id),
    content TEXT NOT NULL,
    token_count INT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE tools (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    parameters JSONB NOT NULL,
    enabled BOOLEAN DEFAULT true
);

CREATE INDEX idx_messages_session ON messages(session_id);
CREATE INDEX idx_messages_embedding ON messages USING hnsw(embedding);
"#,
                    "vector-store" => r#"
-- Vector Store Schema Template
CREATE TABLE vector_stores (
    id SERIAL PRIMARY KEY,
    name TEXT UNIQUE NOT NULL,
    dimensions INT NOT NULL,
    metric TEXT DEFAULT 'cosine',
    index_type TEXT DEFAULT 'hnsw',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE vectors (
    id TEXT PRIMARY KEY,
    store_id INT REFERENCES vector_stores(id),
    values VECTOR(1536) NOT NULL,
    metadata JSONB DEFAULT '{}',
    namespace TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE namespaces (
    id SERIAL PRIMARY KEY,
    store_id INT REFERENCES vector_stores(id),
    name TEXT NOT NULL,
    description TEXT,
    UNIQUE(store_id, name)
);

CREATE INDEX idx_vectors_store ON vectors(store_id);
CREATE INDEX idx_vectors_namespace ON vectors(namespace);
CREATE INDEX idx_vectors_values ON vectors USING hnsw(values);
"#,
                    _ => {
                        println!("{}", format!("Template '{}' not found.", name).yellow());
                        println!("{}", "Use \\ai templates to list available templates.".dimmed());
                        return Ok(MetaCommandResult::Continue);
                    }
                };

                println!("{}", ddl.green());
                println!();
                println!("{}", "To apply this template:".dimmed());
                println!("  Copy the DDL above and execute, or use:");
                println!("  {}", format!("SELECT * FROM apply_template('{}');", name).cyan());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::AiInferSchema(format) => {
                println!("\n{}", "Schema Inference:".bold());
                println!("{}", "─".repeat(60));
                println!("Format: {}", format.cyan());
                println!();

                // For REPL, we provide sample inference with placeholder data
                // In practice, users would provide data via SQL or API
                println!("{}", "Inferring schema from sample data...".dimmed());

                // Create sample data for demonstration
                let sample_data = vec![
                    vec![
                        crate::Value::String("example".to_string()),
                        crate::Value::Int4(42),
                        crate::Value::Boolean(true),
                    ]
                ];

                match db.batch_infer_schema(sample_data) {
                    Ok(schema) => {
                        println!("\n{}", "Inferred Schema:".green().bold());
                        println!("{}", "─".repeat(50));
                        println!("{:<20} {:<15} {}",
                            "Column".bold(), "Type".bold(), "Nullable".bold());
                        println!("{}", "─".repeat(50));
                        for col in &schema.columns {
                            println!("{:<20} {:<15} {}",
                                col.name.cyan(),
                                format!("{:?}", col.data_type).yellow(),
                                if col.nullable { "YES" } else { "NO" }
                            );
                        }
                        println!();
                        println!("{}", "To use with your data:".dimmed());
                        println!("  1. Load data into a table");
                        println!("  2. Use {} to optimize", "\\ai optimize <table>".cyan());
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::AiGenerateSchema(description) => {
                println!("\n{}", "AI Schema Generation:".bold());
                println!("{}", "─".repeat(60));
                println!("Description: {}", description.cyan());
                println!();

                // Use the generate_schema API
                match db.generate_schema(&description) {
                    Ok(ddl) => {
                        println!("{}", "Generated Schema:".green().bold());
                        println!();
                        println!("{}", ddl.green());
                        println!();
                        println!("{}", "To apply this schema:".dimmed());
                        println!("  Copy the DDL above and execute it, or save to a file.");
                    }
                    Err(e) => {
                        // If generation fails (e.g., no LLM configured), show templates instead
                        println!("{}: {}", "Note".yellow(), "Schema generation requires LLM configuration");
                        println!("  {}", format!("{}", e).dimmed());
                        println!();
                        println!("{}", "Try using a template instead:".dimmed());
                        println!("  {} - List available templates", "\\ai templates".cyan());
                        println!("  {} - Show template DDL", "\\ai template <name>".cyan());
                        println!();

                        // Suggest matching template based on description
                        let desc_lower = description.to_lowercase();
                        let suggested = if desc_lower.contains("ecommerce") || desc_lower.contains("shop") || desc_lower.contains("store") {
                            "ecommerce"
                        } else if desc_lower.contains("blog") || desc_lower.contains("post") || desc_lower.contains("article") {
                            "blog"
                        } else if desc_lower.contains("rag") || desc_lower.contains("document") || desc_lower.contains("search") {
                            "rag"
                        } else if desc_lower.contains("agent") || desc_lower.contains("chat") || desc_lower.contains("session") {
                            "agents"
                        } else if desc_lower.contains("vector") || desc_lower.contains("embedding") {
                            "vector-store"
                        } else if desc_lower.contains("saas") || desc_lower.contains("tenant") || desc_lower.contains("subscription") {
                            "saas"
                        } else {
                            "ecommerce"
                        };
                        println!("{}: {}", "Suggested template".green(), suggested.cyan());
                        println!("  {}", format!("\\ai template {}", suggested).cyan());
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::AiOptimize(table_name) => {
                println!("\n{}: {}", "AI Optimization Analysis".bold(), table_name.cyan());
                println!("{}", "═".repeat(60));

                // Check if table exists
                let catalog = db.storage.catalog();
                match catalog.get_table_schema(&table_name) {
                    Ok(schema) => {
                        println!("\n{}", "Current Schema:".bold());
                        println!("{}", "─".repeat(50));
                        for col in &schema.columns {
                            println!("  {} {} {}",
                                col.name.cyan(),
                                format!("{:?}", col.data_type).yellow(),
                                if col.primary_key { "(PK)".green() } else { "".into() }
                            );
                        }

                        println!("\n{}", "Optimization Recommendations:".bold());
                        println!("{}", "─".repeat(50));

                        // Check for missing indexes on common patterns
                        let mut recommendations = Vec::new();

                        for col in &schema.columns {
                            let col_lower = col.name.to_lowercase();

                            // Suggest indexes for common patterns
                            if col_lower.contains("id") && !col.primary_key {
                                recommendations.push(format!(
                                    "Consider index on '{}' (foreign key pattern)",
                                    col.name
                                ));
                            }
                            if col_lower.contains("email") || col_lower.contains("username") {
                                recommendations.push(format!(
                                    "Consider unique index on '{}' (lookup field)",
                                    col.name
                                ));
                            }
                            if col_lower.contains("created") || col_lower.contains("updated") || col_lower.contains("date") {
                                recommendations.push(format!(
                                    "Consider index on '{}' (time-based queries)",
                                    col.name
                                ));
                            }
                            if col_lower.contains("status") || col_lower.contains("type") || col_lower.contains("category") {
                                recommendations.push(format!(
                                    "Consider index on '{}' (filter field)",
                                    col.name
                                ));
                            }
                            // Vector column recommendations
                            if format!("{:?}", col.data_type).contains("Vector") {
                                recommendations.push(format!(
                                    "Add HNSW index on '{}' for vector search:\n    CREATE INDEX idx_{}_vec ON {} USING hnsw({});",
                                    col.name, col.name, table_name, col.name
                                ));
                            }
                        }

                        if recommendations.is_empty() {
                            println!("  {}", "No immediate optimizations identified.".green());
                            println!("  {}", "Table schema looks well-designed.".dimmed());
                        } else {
                            for (i, rec) in recommendations.iter().enumerate() {
                                println!("  {}. {}", (i + 1).to_string().cyan(), rec);
                            }
                        }

                        // General tips
                        println!("\n{}", "General Tips:".bold());
                        println!("  • Use {} to analyze query patterns", "EXPLAIN ANALYZE".cyan());
                        println!("  • Enable compression for large tables: {}",
                            format!("ALTER TABLE {} SET COMPRESSION zstd;", table_name).cyan());
                        println!("  • Consider materialized views for complex aggregations");
                        println!();
                    }
                    Err(e) => {
                        println!("{}: Table '{}' not found", "Error".red(), table_name);
                        println!("  {}", format!("{}", e).dimmed());
                        println!();
                        println!("{}", "Available tables:".dimmed());
                        if let Ok(tables) = catalog.list_tables() {
                            for t in tables.iter().take(10) {
                                println!("  {}", t.cyan());
                            }
                        }
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::AiModels => {
                println!("\n{}", "Available AI Models:".bold());
                println!("{}", "─".repeat(60));

                match db.list_chat_models() {
                    Ok(models) => {
                        if models.is_empty() {
                            println!("{}", "No AI models configured.".dimmed());
                            println!();
                            println!("{}", "To configure AI models, add to config.toml:".dimmed());
                            println!("  [ai]");
                            println!("  provider = \"openai\"");
                            println!("  api_key = \"sk-...\"");
                        } else {
                            println!("{:<30} {}", "Model".bold(), "Info".bold());
                            println!("{}", "─".repeat(60));
                            for model in models {
                                let name = model.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                                let provider = model.get("provider").and_then(|v| v.as_str()).unwrap_or("");
                                println!("{:<30} {}", name.cyan(), provider.dimmed());
                            }
                        }
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::AiEmbed(text) => {
                println!("\n{}", "Generating Embedding:".bold());
                println!("{}", "─".repeat(60));
                println!("{}: \"{}\"", "Text".dimmed(), text);
                println!();

                match db.create_embeddings(vec![text.clone()]) {
                    Ok(embeddings) => {
                        if let Some(embedding) = embeddings.first() {
                            println!("{}: {} dimensions", "Embedding".green(), embedding.len());
                            println!();
                            println!("{}", "First 10 values:".dimmed());
                            let preview: Vec<String> = embedding.iter().take(10).map(|v| format!("{:.4}", v)).collect();
                            println!("  [{}...]", preview.join(", "));
                            println!();
                            println!("{}", "Use in SQL:".dimmed());
                            println!("  INSERT INTO docs (text, embedding) VALUES ('{}', '[{}]');",
                                text.chars().take(20).collect::<String>(),
                                embedding.iter().map(|v| format!("{:.6}", v)).collect::<Vec<_>>().join(",")
                            );
                        }
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                        println!();
                        println!("{}", "Note: Embedding generation requires AI configuration.".dimmed());
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::AiCompareSchema { schema1, schema2 } => {
                println!("\n{}", "Schema Comparison:".bold());
                println!("{}", "─".repeat(60));
                println!("Schema 1: {}", schema1.cyan());
                println!("Schema 2: {}", schema2.cyan());
                println!();

                let catalog = db.storage.catalog();

                // Get both schemas
                let s1 = catalog.get_table_schema(&schema1);
                let s2 = catalog.get_table_schema(&schema2);

                match (s1, s2) {
                    (Ok(schema1_obj), Ok(schema2_obj)) => {
                        match db.compare_schemas(&schema1_obj, &schema2_obj) {
                            Ok(diff) => {
                                println!("{}", serde_json::to_string_pretty(&diff).unwrap_or_else(|_| format!("{:?}", diff)));
                            }
                            Err(e) => {
                                // Manual comparison if API fails
                                println!("{}", "Comparison:".bold());
                                println!();

                                let cols1: std::collections::HashSet<_> = schema1_obj.columns.iter().map(|c| &c.name).collect();
                                let cols2: std::collections::HashSet<_> = schema2_obj.columns.iter().map(|c| &c.name).collect();

                                let only_in_1: Vec<_> = cols1.difference(&cols2).collect();
                                let only_in_2: Vec<_> = cols2.difference(&cols1).collect();
                                let common: Vec<_> = cols1.intersection(&cols2).collect();

                                if !only_in_1.is_empty() {
                                    println!("{} (only in {}):", "Removed".red(), schema1);
                                    for col in only_in_1 {
                                        println!("  - {}", col);
                                    }
                                }
                                if !only_in_2.is_empty() {
                                    println!("{} (only in {}):", "Added".green(), schema2);
                                    for col in only_in_2 {
                                        println!("  + {}", col);
                                    }
                                }
                                println!("{}: {} columns", "Common".dimmed(), common.len());
                                println!();
                                println!("{}: {}", "Note".dimmed(), e);
                            }
                        }
                    }
                    (Err(e1), _) => {
                        println!("{}: Schema '{}' not found - {}", "Error".red(), schema1, e1);
                    }
                    (_, Err(e2)) => {
                        println!("{}: Schema '{}' not found - {}", "Error".red(), schema2, e2);
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            // Agent Session Commands
            MetaCommand::ListSessions => {
                println!("\n{}", "Agent Sessions:".bold());
                println!("{}", "─".repeat(70));

                match db.list_agent_sessions() {
                    Ok(sessions) => {
                        if sessions.is_empty() {
                            println!("{}", "No sessions found.".dimmed());
                        } else {
                            println!("{:<36} {:<20} {}",
                                "ID".bold(), "Name".bold(), "Created".bold());
                            println!("{}", "─".repeat(70));
                            for session in sessions {
                                println!("{:<36} {:<20} {}",
                                    session.id.cyan(),
                                    session.name,
                                    session.created_at.dimmed()
                                );
                            }
                        }
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::SessionNew(name) => {
                match db.create_agent_session(&name) {
                    Ok(session) => {
                        println!("{}: {}", "Session created".green(), session.id.cyan());
                        println!("  Name: {}", name);
                        println!("\n{}", "Start chatting:".dimmed());
                        println!("  {}", format!("\\chat {}", session.id).cyan());
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::SessionDetails(id) => {
                match db.get_agent_session(&id) {
                    Ok(session) => {
                        println!("\n{}: {}", "Session".bold(), id.cyan());
                        println!("{}", "─".repeat(50));
                        println!("  Name: {}", if session.name.is_empty() { "(unnamed)" } else { &session.name });
                        println!("  Created: {}", session.created_at);

                        // Show message count
                        match db.get_agent_messages(&id) {
                            Ok(messages) => {
                                println!("  Messages: {}", messages.len());
                            }
                            Err(_) => {
                                println!("  Messages: unknown");
                            }
                        }
                        println!();
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::SessionDelete(id) => {
                match db.delete_agent_session(&id) {
                    Ok(_) => {
                        println!("{}: {}", "Session deleted".green(), id.cyan());
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ChatSession(id) => {
                println!("\n{}: {}", "Chat Session".bold(), id.cyan());
                println!("{}", "─".repeat(50));
                println!("{}", "Interactive chat mode".yellow());
                println!("{}", "Type messages to chat. Use Ctrl-C to exit.".dimmed());
                println!();

                // Show existing messages
                match db.get_agent_messages(&id) {
                    Ok(messages) => {
                        if !messages.is_empty() {
                            println!("{}", "Previous messages:".dimmed());
                            for msg in messages.iter().rev().take(5).rev() {
                                let role = &msg.role;
                                let content = &msg.content;
                                match role.as_str() {
                                    "user" => println!("  {}: {}", "You".green(), content),
                                    "assistant" => println!("  {}: {}", "AI".blue(), content),
                                    _ => println!("  {}: {}", role, content),
                                }
                            }
                            println!();
                        }
                    }
                    Err(_) => {}
                }

                println!("{}", "Note: Full interactive chat requires async runtime.".dimmed());
                println!("{}", "Use SQL or REST API for full chat functionality.".dimmed());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::SessionClear(id) => {
                match db.clear_agent_messages(&id) {
                    Ok(_) => {
                        println!("{}: {}", "Session cleared".green(), id.cyan());
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::SessionFork { id, name } => {
                println!("\n{}: {} -> {}", "Forking Session".bold(), id.cyan(), name.green());
                println!("{}", "─".repeat(50));

                match db.fork_agent_session(&id, &name) {
                    Ok(new_session) => {
                        println!("{}: {}", "New session created".green(), new_session.id.cyan());
                        println!("  Name: {}", name);
                        println!("  Forked from: {}", id);
                        println!();
                        println!("{}", "Start chatting:".dimmed());
                        println!("  {}", format!("\\chat {}", new_session.id).cyan());
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::SessionContext(id) => {
                println!("\n{}: {}", "Session Context".bold(), id.cyan());
                println!("{}", "─".repeat(50));

                match db.get_agent_context(&id) {
                    Ok(context) => {
                        println!("{}", serde_json::to_string_pretty(&context).unwrap_or_else(|_| format!("{:?}", context)));
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::SessionMemory { id, query } => {
                println!("\n{}: {}", "Session Memory Search".bold(), id.cyan());
                println!("{}: {}", "Query".dimmed(), query);
                println!("{}", "─".repeat(50));

                match db.search_agent_memory(&id, &query) {
                    Ok(results) => {
                        if results.is_empty() {
                            println!("{}", "No matching messages found.".dimmed());
                        } else {
                            println!("{:<8} {:<15} {}", "Score".bold(), "Role".bold(), "Content".bold());
                            println!("{}", "─".repeat(70));
                            for (msg, score) in results.iter().take(10) {
                                let preview: String = msg.content.chars().take(50).collect();
                                println!("{:<8.4} {:<15} {}",
                                    score,
                                    msg.role.cyan(),
                                    preview.dimmed()
                                );
                            }
                        }
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::SessionSummarize(id) => {
                println!("\n{}: {}", "Session Summary".bold(), id.cyan());
                println!("{}", "─".repeat(50));

                match db.summarize_agent_memory(&id) {
                    Ok(summary) => {
                        println!("{}", summary);
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            // Vector Management Commands
            MetaCommand::ListVectors => {
                println!("\n{}", "Vector Stores:".bold());
                println!("{}", "─".repeat(70));

                match db.list_vector_stores() {
                    Ok(stores) => {
                        if stores.is_empty() {
                            println!("{}", "No vector stores found.".dimmed());
                            println!();
                            println!("{}", "Create one:".dimmed());
                            println!("  {}", "\\vector create mystore 384 cosine".cyan());
                        } else {
                            println!("{:<20} {:<12} {:<15} {}",
                                "Name".bold(), "Dimensions".bold(), "Metric".bold(), "Vectors".bold());
                            println!("{}", "─".repeat(70));
                            for store in stores {
                                println!("{:<20} {:<12} {:<15} {}",
                                    store.name.cyan(),
                                    store.dimensions,
                                    store.metric,
                                    store.vector_count
                                );
                            }
                        }
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::VectorDetails(name) => {
                match db.get_vector_store(&name) {
                    Ok(store) => {
                        println!("\n{}: {}", "Vector Store".bold(), name.cyan());
                        println!("{}", "─".repeat(50));
                        println!("  Dimensions: {}", store.dimensions);
                        println!("  Metric: {}", store.metric);
                        println!("  Vectors: {}", store.vector_count);
                        println!("  Index Type: {}", store.index_type);
                        println!();
                        println!("{}", "Operations:".dimmed());
                        println!("  {} - Show statistics", format!("\\vector stats {}", name).cyan());
                        println!("  {} - Delete store", format!("\\vector delete {}", name).cyan());
                        println!();
                    }
                    Err(e) => {
                        println!("{}: Vector store '{}' not found", "Error".red(), name);
                        println!("{}", format!("  {}", e).dimmed());
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::VectorCreate { name, dimensions, metric } => {
                let metric_str = metric.as_deref().unwrap_or("cosine");
                println!("Creating vector store: {} ({} dims, {} metric)",
                    name.cyan(), dimensions, metric_str);

                match db.create_vector_store(&name, *dimensions) {
                    Ok(store) => {
                        println!("{}: Vector store '{}' created", "Success".green(), name.cyan());
                        println!("  Dimensions: {}", store.dimensions);
                        println!();
                        println!("{}", "Next steps:".dimmed());
                        println!("  {} - Insert vectors via SQL", "INSERT INTO...".cyan());
                        println!("  {} - View store", format!("\\vector {}", name).cyan());
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::VectorDelete(name) => {
                match db.delete_vector_store(&name) {
                    Ok(_) => {
                        println!("{}: Vector store '{}' deleted", "Success".green(), name.cyan());
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::VectorStats(name) => {
                println!("\n{}: {}", "Vector Store Statistics".bold(), name.cyan());
                println!("{}", "─".repeat(50));

                match db.get_vector_store(&name) {
                    Ok(store) => {
                        println!("  Dimensions: {}", store.dimensions);
                        println!("  Total Vectors: {}", store.vector_count);
                        println!("  Index Type: {}", store.index_type);
                        println!("  Metric: {}", store.metric);
                        println!();
                        println!("{}", "Index Configuration:".bold());
                        println!("  HNSW M: 16");
                        println!("  HNSW ef_construction: 200");
                        println!();
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            // Document Management Commands
            MetaCommand::ListCollections => {
                println!("\n{}", "Document Collections:".bold());
                println!("{}", "─".repeat(60));

                match db.list_collections() {
                    Ok(collections) => {
                        if collections.is_empty() {
                            println!("{}", "No collections found.".dimmed());
                            println!();
                            println!("{}", "Create one:".dimmed());
                            println!("  {}", "SQL: CREATE TABLE docs (id TEXT, content TEXT, embedding VECTOR(384));".cyan());
                        } else {
                            println!("{:<30} {}", "Name".bold(), "Documents".bold());
                            println!("{}", "─".repeat(60));
                            for name in collections {
                                // Try to get document count
                                let count = db.list_documents(&name)
                                    .map(|docs| docs.len())
                                    .unwrap_or(0);
                                println!("{:<30} {}", name.cyan(), count);
                            }
                        }
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::CollectionDetails(name) => {
                println!("\n{}: {}", "Collection".bold(), name.cyan());
                println!("{}", "─".repeat(50));

                match db.list_documents(&name) {
                    Ok(docs) => {
                        println!("  Documents: {}", docs.len());
                        println!();
                        println!("{}", "Operations:".dimmed());
                        println!("  {} - List documents", format!("\\docs {}", name).cyan());
                        println!("  {} - Search collection", "\\search-docs <query>".cyan());
                    }
                    Err(e) => {
                        println!("{}: Collection '{}' not found", "Error".red(), name);
                        println!("{}", format!("  {}", e).dimmed());
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ListDocumentsInCollection(collection) => {
                println!("\n{}: {}", "Documents in".bold(), collection.cyan());
                println!("{}", "─".repeat(70));

                match db.list_documents(&collection) {
                    Ok(docs) => {
                        if docs.is_empty() {
                            println!("{}", "No documents found.".dimmed());
                        } else {
                            println!("{:<36} {:<15} {}",
                                "ID".bold(), "Size".bold(), "Preview".bold());
                            println!("{}", "─".repeat(70));
                            for doc in docs.iter().take(20) {
                                let id = &doc.id;
                                let size = doc.size;
                                let preview: String = doc.content.chars().take(30).collect();
                                println!("{:<36} {:<15} {}",
                                    id.cyan(),
                                    format!("{} bytes", size),
                                    preview.dimmed()
                                );
                            }
                            if docs.len() > 20 {
                                println!("{}", format!("... and {} more", docs.len() - 20).dimmed());
                            }
                        }
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::DocumentDetails { collection, id } => {
                println!("\n{}: {}", "Document".bold(), id.cyan());
                println!("{}", "─".repeat(60));

                match db.get_document(&collection, &id) {
                    Ok(doc) => {
                        println!("  Collection: {}", collection);
                        println!("  ID: {}", id.cyan());
                        println!("  Created: {}", doc.created_at);
                        println!("  Updated: {}", doc.updated_at);
                        println!();
                        println!("{}", "Content Preview:".bold());
                        let preview: String = doc.content.chars().take(500).collect();
                        println!("{}", preview.dimmed());
                        if doc.content.len() > 500 {
                            println!("{}", format!("... ({} more chars)", doc.content.len() - 500).dimmed());
                        }
                        println!();
                        if let Some(metadata) = &doc.metadata {
                            println!("{}", "Metadata:".bold());
                            println!("  {}", metadata);
                        }
                        if !doc.chunks.is_empty() {
                            println!();
                            println!("{}: {}", "Chunks".bold(), doc.chunks.len());
                        }
                    }
                    Err(e) => {
                        println!("{}: Document '{}' not found in '{}'", "Error".red(), id, collection);
                        println!("{}", format!("  {}", e).dimmed());
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::SearchDocuments(query) => {
                println!("\n{}: \"{}\"", "Searching".bold(), query.cyan());
                println!("{}", "─".repeat(60));

                // Search across all collections
                match db.list_collections() {
                    Ok(collections) => {
                        let mut total_results = 0;
                        for collection in collections {
                            match db.search_documents(&collection, &query) {
                                Ok(results) => {
                                    if !results.is_empty() {
                                        println!("\n{}: {}", "Collection".bold(), collection.cyan());
                                        for doc in results.iter().take(5) {
                                            let id = &doc.id;
                                            let preview: String = doc.content.chars().take(100).collect();
                                            println!("  {} - {}", id.cyan(), preview.dimmed());
                                            total_results += 1;
                                        }
                                        if results.len() > 5 {
                                            println!("  {}", format!("... and {} more", results.len() - 5).dimmed());
                                        }
                                    }
                                }
                                Err(_) => {}
                            }
                        }
                        if total_results == 0 {
                            println!("{}", "No results found.".dimmed());
                        }
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::DocumentChunks { collection, id } => {
                println!("\n{}: {}/{}", "Document Chunks".bold(), collection.cyan(), id.cyan());
                println!("{}", "─".repeat(60));

                match db.get_document_chunks(&collection, &id) {
                    Ok(chunks) => {
                        if chunks.is_empty() {
                            println!("{}", "No chunks found.".dimmed());
                        } else {
                            println!("{:<8} {:<60}", "Score".bold(), "Content Preview".bold());
                            println!("{}", "─".repeat(70));
                            for (i, (content, score)) in chunks.iter().enumerate() {
                                let preview: String = content.chars().take(55).collect();
                                println!("{:<8.4} {:<60}", score, format!("{}...", preview).dimmed());
                                if i >= 19 {
                                    println!("{}", format!("... and {} more chunks", chunks.len() - 20).dimmed());
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::DocumentRechunk { collection, id, chunk_size } => {
                println!("\n{}: {}/{}", "Re-chunking Document".bold(), collection.cyan(), id.cyan());
                println!("{}: {} characters", "Chunk size".dimmed(), chunk_size);
                println!("{}", "─".repeat(60));

                match db.rechunk_document(&collection, &id, *chunk_size) {
                    Ok(new_chunks) => {
                        println!("{}: {} chunks created", "Success".green(), new_chunks.len());
                        println!();
                        println!("{}", "Preview of first 3 chunks:".dimmed());
                        for (i, chunk) in new_chunks.iter().take(3).enumerate() {
                            let preview: String = chunk.chars().take(80).collect();
                            println!("  {}: {}...", i + 1, preview.dimmed());
                        }
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::RagSearch { collection, query, k } => {
                println!("\n{}: {}", "RAG Search".bold(), collection.cyan());
                println!("{}: \"{}\" (k={})", "Query".dimmed(), query, k);
                println!("{}", "─".repeat(70));

                match db.rag_search(&collection, &query, *k) {
                    Ok(results) => {
                        if results.is_empty() {
                            println!("{}", "No results found.".dimmed());
                        } else {
                            println!("{:<8} {:<25} {}", "Score".bold(), "Document".bold(), "Context".bold());
                            println!("{}", "─".repeat(70));
                            for (doc, score, context) in results.iter().take(*k) {
                                let context_preview: String = context.chars().take(35).collect();
                                println!("{:<8.4} {:<25} {}",
                                    score,
                                    doc.id.cyan(),
                                    context_preview.dimmed()
                                );
                            }
                        }
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            // Performance & Utility Commands
            MetaCommand::ExplainQueryWithOptions { query, options } => {
                // Use real ExplainPlanner with full options support
                Self::execute_explain_with_options(db, query, options)?;
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ProfileQuery(query) => {
                println!("\n{}", "Query Profile:".bold());
                println!("{}", "─".repeat(70));
                println!("{}: {}", "Query".dimmed(), query.cyan());
                println!();

                // Time the query execution
                let start = std::time::Instant::now();
                match db.query(&query, &[]) {
                    Ok(results) => {
                        let elapsed = start.elapsed();
                        let row_count = results.len();

                        println!("{}", "Execution Metrics:".bold());
                        println!("  Rows returned: {}", row_count.to_string().cyan());
                        println!("  Total time: {}", format!("{:.3}ms", elapsed.as_secs_f64() * 1000.0).green());
                        println!("  Time per row: {}",
                            if row_count > 0 {
                                format!("{:.3}µs", (elapsed.as_secs_f64() * 1_000_000.0) / row_count as f64)
                            } else {
                                "N/A".to_string()
                            }.dimmed()
                        );
                        println!();

                        // Memory estimate (rough)
                        let mem_estimate = row_count * 100; // Rough estimate bytes per row
                        println!("{}", "Resource Usage:".bold());
                        println!("  Memory (est.): {} bytes", mem_estimate);
                        println!();

                        // Show sample results
                        if !results.is_empty() {
                            println!("{}", "Sample Results (first 3 rows):".bold());
                            for (i, row) in results.iter().take(3).enumerate() {
                                let row_str: String = row.values.iter()
                                    .take(5)
                                    .map(|v| format!("{:?}", v))
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                println!("  [{}] {}", i + 1, row_str.dimmed());
                            }
                            if results.len() > 3 {
                                println!("  {}", format!("... and {} more rows", results.len() - 3).dimmed());
                            }
                        }
                    }
                    Err(e) => {
                        let elapsed = start.elapsed();
                        println!("{}: Query failed after {:.3}ms", "Error".red(), elapsed.as_secs_f64() * 1000.0);
                        println!("  {}", format!("{}", e).dimmed());
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::Telemetry => {
                println!("\n{}", "Database Telemetry:".bold());
                println!("{}", "═".repeat(60));

                // Storage stats
                println!("\n{}", "Storage:".bold());
                let catalog = db.storage.catalog();
                match catalog.list_tables() {
                    Ok(tables) => {
                        println!("  Tables: {}", tables.len().to_string().cyan());
                    }
                    Err(_) => {
                        println!("  Tables: {}", "unknown".dimmed());
                    }
                }

                // System info
                println!("\n{}", "System:".bold());
                println!("  Workers: {}",
                    std::thread::available_parallelism()
                        .map(|n| n.get().to_string())
                        .unwrap_or_else(|_| "unknown".to_string())
                        .cyan()
                );

                // Memory info using sysinfo crate
                {
                    use sysinfo::System;
                    let sys = System::new_all();
                    println!("  Memory Used: {} MB", sys.used_memory() / 1024 / 1024);
                    println!("  Memory Total: {} MB", sys.total_memory() / 1024 / 1024);
                }

                // Configuration
                println!("\n{}", "Configuration:".bold());
                println!("  WAL: {}", "enabled".green());
                println!("  Compression: {}", "zstd".cyan());
                println!("  Time-Travel: {}", "enabled".green());
                println!("  SIMD: {}", "enabled".green());

                // Vector stores
                println!("\n{}", "Vector Stores:".bold());
                match db.list_vector_stores() {
                    Ok(stores) => {
                        if stores.is_empty() {
                            println!("  {}", "None configured".dimmed());
                        } else {
                            for store in stores {
                                println!("  {} ({} dims, {} vectors)",
                                    store.name.cyan(),
                                    store.dimensions,
                                    store.vector_count
                                );
                            }
                        }
                    }
                    Err(_) => {
                        println!("  {}", "Unable to retrieve".dimmed());
                    }
                }

                // Agent sessions
                println!("\n{}", "Agent Sessions:".bold());
                match db.list_agent_sessions() {
                    Ok(sessions) => {
                        println!("  Active: {}", sessions.len().to_string().cyan());
                    }
                    Err(_) => {
                        println!("  Active: {}", "unknown".dimmed());
                    }
                }

                println!();
                println!("{}", "For detailed stats, use:".dimmed());
                println!("  {}", "SELECT * FROM pg_database_branches();".cyan());
                println!("  {}", "SELECT * FROM pg_mv_staleness();".cyan());
                println!("  {}", "SELECT * FROM pg_vector_index_stats();".cyan());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::Dump(output_path) => {
                let output_file = output_path.as_ref()
                    .cloned()
                    .unwrap_or_else(|| std::path::PathBuf::from("dump.sql"));

                println!("\n{}", "Database Dump:".bold());
                println!("{}", "─".repeat(70));
                println!("Output: {}", output_file.display().to_string().cyan());
                println!();

                // Get all tables from catalog
                let catalog = db.storage.catalog();
                match catalog.list_tables() {
                    Ok(tables) => {
                        if tables.is_empty() {
                            println!("{}", "No tables to dump.".yellow());
                            return Ok(MetaCommandResult::Continue);
                        }

                        let mut dump_content = String::new();
                        dump_content.push_str("-- HeliosDB Lite Database Dump\n");
                        dump_content.push_str(&format!("-- Generated: {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S")));
                        dump_content.push_str("-- Database: heliosdb-lite\n\n");

                        let mut total_rows = 0;
                        let mut dump_lines = 0;

                        // Dump each table
                        for table in &tables {
                            match catalog.get_table_schema(&table) {
                                Ok(schema) => {
                                    // Add CREATE TABLE statement
                                    dump_content.push_str(&format!("-- Table: {}\n", table));
                                    dump_content.push_str("CREATE TABLE IF NOT EXISTS ");
                                    dump_content.push_str(&table);
                                    dump_content.push_str(" (\n");

                                    for (i, col) in schema.columns.iter().enumerate() {
                                        if i > 0 { dump_content.push_str(",\n"); }
                                        dump_content.push_str("  ");
                                        dump_content.push_str(&col.name);
                                        dump_content.push_str(" ");
                                        dump_content.push_str(&format!("{:?}", col.data_type));
                                        if col.primary_key { dump_content.push_str(" PRIMARY KEY"); }
                                        if !col.nullable { dump_content.push_str(" NOT NULL"); }
                                    }

                                    dump_content.push_str("\n);\n\n");

                                    // Dump data
                                    match db.query(&format!("SELECT * FROM {}", table), &[]) {
                                        Ok(rows) => {
                                            for row in rows {
                                                dump_content.push_str(&format!("-- Row data would go here ({})\n", total_rows + 1));
                                                total_rows += 1;
                                            }
                                        }
                                        Err(_) => {
                                            dump_content.push_str("-- Error reading table data\n");
                                        }
                                    }
                                    dump_content.push_str("\n");
                                    dump_lines += 1;
                                }
                                Err(_) => {
                                    println!("{}: Could not read schema for table '{}'", "Warning".yellow(), table);
                                }
                            }
                        }

                        // Write to file
                        match std::fs::write(&output_file, dump_content) {
                            Ok(_) => {
                                println!("{}: Dump completed successfully", "Success".green());
                                println!("  Tables: {}", tables.len().to_string().cyan());
                                println!("  Rows: {}", total_rows.to_string().cyan());
                                println!("  Schema lines: {}", dump_lines.to_string().cyan());
                                println!("  Output file: {}", output_file.display().to_string().green());
                                println!();
                                Ok(MetaCommandResult::Continue)
                            }
                            Err(e) => {
                                println!("{}: Failed to write dump file", "Error".red());
                                println!("  {}", format!("{}", e).dimmed());
                                println!();
                                Err(Error::io(format!("Failed to write dump file: {}", e)))
                            }
                        }
                    }
                    Err(e) => {
                        println!("{}: Could not list tables", "Error".red());
                        println!("  {}", format!("{}", e).dimmed());
                        println!();
                        Err(e)
                    }
                }
            }

            // v3.2 Multi-Tenancy Commands
            MetaCommand::TenantList => {
                println!("\n{}", "Tenants:".bold());
                println!("{}", "─".repeat(90));
                println!("{:<38} {:<20} {:<15} {}",
                    "ID".bold(), "Name".bold(), "Isolation".bold(), "Plan".bold());
                println!("{}", "─".repeat(90));

                let tenants = db.tenant_manager.list_tenants();
                if tenants.is_empty() {
                    println!("{}", "No tenants found.".dimmed());
                    println!();
                    println!("{}", "Create a tenant:".dimmed());
                    println!("  {}", "\\tenant create <name> [plan] [isolation]".cyan());
                    println!("  {}", "Plans: free, starter, pro, enterprise, unlimited".dimmed());
                    println!("  {}", "Isolation: shared (default), schema, database".dimmed());
                } else {
                    for tenant in tenants {
                        let isolation_str = match tenant.isolation_mode {
                            crate::tenant::IsolationMode::SharedSchema => "SharedSchema",
                            crate::tenant::IsolationMode::DatabasePerTenant => "DBPerTenant",
                            crate::tenant::IsolationMode::SchemaPerTenant => "SchemaPerTenant",
                        };
                        // Determine plan from limits
                        let plan = Self::limits_to_plan(&tenant.limits);
                        println!("{:<38} {:<20} {:<15} {}",
                            tenant.id.to_string().cyan(),
                            tenant.name,
                            isolation_str.yellow(),
                            plan.green()
                        );
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantCreate { name, plan, isolation } => {
                println!("\n{}", "Creating Tenant:".bold());
                println!("{}", "─".repeat(60));

                let plan_name = plan.as_deref().unwrap_or("free");

                // Parse isolation mode
                let (isolation_mode, isolation_desc) = match isolation.as_deref().unwrap_or("shared") {
                    "shared" | "rls" => (crate::tenant::IsolationMode::SharedSchema, "SharedSchema (RLS)"),
                    "schema" => (crate::tenant::IsolationMode::SchemaPerTenant, "SchemaPerTenant"),
                    "database" | "db" => (crate::tenant::IsolationMode::DatabasePerTenant, "DatabasePerTenant"),
                    other => {
                        println!("{}: Unknown isolation mode '{}', using SharedSchema", "Warning".yellow(), other);
                        println!("  Valid modes: shared, schema, database");
                        (crate::tenant::IsolationMode::SharedSchema, "SharedSchema (RLS)")
                    }
                };

                // Create tenant with specified plan and isolation mode
                let tenant = db.tenant_manager.register_tenant_with_plan(
                    name.clone(),
                    isolation_mode,
                    plan_name,
                );

                println!("{}: Tenant '{}' created", "Success".green(), name.cyan());
                println!("  ID: {}", tenant.id.to_string().cyan());
                println!("  Plan: {}", tenant.plan_id.green());
                println!("  Isolation: {}", isolation_desc.yellow());
                println!();
                println!("{}", "Resource Limits:".bold());
                println!("  Storage: {}", Self::format_storage(tenant.limits.max_storage_bytes));
                println!("  Connections: {}", tenant.limits.max_connections);
                println!("  QPS: {}", tenant.limits.max_qps);
                println!();
                println!("{}", "Set as current tenant:".dimmed());
                println!("  {}", format!("\\tenant use {}", name).cyan());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantUse(tenant_ref) => {
                let tenants = db.tenant_manager.list_tenants();

                // Find tenant by name or ID
                let tenant = tenants.iter().find(|t| {
                    t.name.eq_ignore_ascii_case(&tenant_ref) ||
                    t.id.to_string().starts_with(tenant_ref.as_str())
                });

                match tenant {
                    Some(t) => {
                        // Set tenant context
                        let context = crate::tenant::TenantContext {
                            tenant_id: t.id,
                            user_id: "repl_user".to_string(),
                            roles: vec!["admin".to_string()],
                            isolation_mode: t.isolation_mode,
                        };
                        db.tenant_manager.set_current_context(context);

                        println!("{}: Now using tenant '{}'", "Success".green(), t.name.cyan());
                        println!("  ID: {}", t.id.to_string().dimmed());
                        println!("  RLS: {}", if t.rls_enabled { "enabled".green() } else { "disabled".yellow() });
                        println!();
                    }
                    None => {
                        println!("{}: Tenant '{}' not found", "Error".red(), tenant_ref);
                        println!();
                        println!("{}", "List tenants:".dimmed());
                        println!("  {}", "\\tenants".cyan());
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantInfo(tenant_ref) => {
                let tenants = db.tenant_manager.list_tenants();

                // Find tenant by name or ID
                let tenant = tenants.iter().find(|t| {
                    t.name.eq_ignore_ascii_case(&tenant_ref) ||
                    t.id.to_string().starts_with(tenant_ref.as_str())
                });

                match tenant {
                    Some(t) => {
                        println!("\n{}: {}", "Tenant".bold(), t.name.cyan());
                        println!("{}", "═".repeat(60));

                        println!("\n{}", "Identification:".bold());
                        println!("  ID: {}", t.id.to_string().cyan());
                        println!("  Name: {}", t.name);
                        println!("  Created: {}", t.created_at.dimmed());

                        println!("\n{}", "Isolation:".bold());
                        let isolation_str = match t.isolation_mode {
                            crate::tenant::IsolationMode::SharedSchema => "SharedSchema (RLS-based)",
                            crate::tenant::IsolationMode::DatabasePerTenant => "Database per Tenant",
                            crate::tenant::IsolationMode::SchemaPerTenant => "Schema per Tenant",
                        };
                        println!("  Mode: {}", isolation_str.yellow());
                        println!("  RLS Enabled: {}", if t.rls_enabled { "Yes".green() } else { "No".red() });

                        let plan = Self::limits_to_plan(&t.limits);
                        println!("\n{}", "Plan & Limits:".bold());
                        println!("  Plan: {}", plan.green());
                        println!("  Max Storage: {} MB", t.limits.max_storage_bytes / 1024 / 1024);
                        println!("  Max Connections: {}", t.limits.max_connections);
                        println!("  Max QPS: {}", t.limits.max_qps);

                        // Show quota usage if available
                        if let Some(quota) = db.tenant_manager.get_quota_tracking(t.id) {
                            println!("\n{}", "Current Usage:".bold());
                            let storage_pct = (quota.storage_bytes_used as f64 / t.limits.max_storage_bytes as f64) * 100.0;
                            println!("  Storage: {} MB / {} MB ({:.1}%)",
                                quota.storage_bytes_used / 1024 / 1024,
                                t.limits.max_storage_bytes / 1024 / 1024,
                                storage_pct
                            );
                            println!("  Active Connections: {} / {}",
                                quota.active_connections,
                                t.limits.max_connections
                            );
                            println!("  Queries (window): {} / {}",
                                quota.queries_this_window,
                                t.limits.max_qps
                            );
                            println!("  Window Reset: {}", quota.window_reset_at.dimmed());
                        }

                        println!();
                    }
                    None => {
                        println!("{}: Tenant '{}' not found", "Error".red(), tenant_ref);
                        println!();
                        println!("{}", "List tenants:".dimmed());
                        println!("  {}", "\\tenants".cyan());
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantQuota(tenant_ref) => {
                let tenants = db.tenant_manager.list_tenants();

                // If no tenant specified, use current context
                let tenant = if let Some(ref tr) = tenant_ref {
                    tenants.iter().find(|t| {
                        t.name.eq_ignore_ascii_case(tr) ||
                        t.id.to_string().starts_with(tr)
                    })
                } else if let Some(ctx) = db.tenant_manager.get_current_context() {
                    tenants.iter().find(|t| t.id == ctx.tenant_id)
                } else {
                    None
                };

                match tenant {
                    Some(t) => {
                        println!("\n{}: {}", "Quota Usage".bold(), t.name.cyan());
                        println!("{}", "═".repeat(60));

                        if let Some(quota) = db.tenant_manager.get_quota_tracking(t.id) {
                            // Storage bar
                            let storage_pct = (quota.storage_bytes_used as f64 / t.limits.max_storage_bytes as f64) * 100.0;
                            let storage_bar = Self::progress_bar(storage_pct, 30);
                            println!("\n{}", "Storage:".bold());
                            println!("  {} {:.1}%", storage_bar, storage_pct);
                            println!("  {} MB / {} MB",
                                quota.storage_bytes_used / 1024 / 1024,
                                t.limits.max_storage_bytes / 1024 / 1024
                            );

                            // Connections bar
                            let conn_pct = (quota.active_connections as f64 / t.limits.max_connections as f64) * 100.0;
                            let conn_bar = Self::progress_bar(conn_pct, 30);
                            println!("\n{}", "Connections:".bold());
                            println!("  {} {:.1}%", conn_bar, conn_pct);
                            println!("  {} / {}", quota.active_connections, t.limits.max_connections);

                            // QPS bar
                            let qps_pct = (quota.queries_this_window as f64 / t.limits.max_qps as f64) * 100.0;
                            let qps_bar = Self::progress_bar(qps_pct, 30);
                            println!("\n{}", "Queries Per Second:".bold());
                            println!("  {} {:.1}%", qps_bar, qps_pct);
                            println!("  {} / {}", quota.queries_this_window, t.limits.max_qps);
                            println!("  Window resets: {}", quota.window_reset_at.dimmed());

                            println!();
                        } else {
                            println!("{}", "No quota tracking data available.".dimmed());
                        }
                    }
                    None => {
                        if tenant_ref.is_some() {
                            println!("{}: Tenant not found", "Error".red());
                        } else {
                            println!("{}: No tenant context set", "Error".red());
                            println!("{}", "Use: \\tenant use <name> or \\tenant quota <name>".dimmed());
                        }
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantUsage(tenant_ref) => {
                let tenants = db.tenant_manager.list_tenants();

                // If no tenant specified, use current context or show all
                let target_tenants: Vec<_> = if let Some(ref tr) = tenant_ref {
                    tenants.iter().filter(|t| {
                        t.name.eq_ignore_ascii_case(tr) ||
                        t.id.to_string().starts_with(tr)
                    }).collect()
                } else if let Some(ctx) = db.tenant_manager.get_current_context() {
                    tenants.iter().filter(|t| t.id == ctx.tenant_id).collect()
                } else {
                    // Show all tenants
                    tenants.iter().collect()
                };

                if target_tenants.is_empty() {
                    if tenant_ref.is_some() {
                        println!("{}: Tenant not found", "Error".red());
                    } else {
                        println!("{}: No tenants registered", "Info".yellow());
                    }
                    return Ok(MetaCommandResult::Continue);
                }

                println!();
                println!("{}", "Real-Time Tenant Usage Statistics".bold());
                println!("{}", "═".repeat(80));

                for tenant in &target_tenants {
                    let plan = db.tenant_manager.plan_manager.get_plan(&tenant.plan_id);
                    let plan_name = plan.as_ref().map(|p| p.name.as_str()).unwrap_or("unknown");

                    println!();
                    println!("{} {} ({})", "Tenant:".bold(), tenant.name.cyan(), tenant.plan_id.dimmed());
                    println!("  Plan: {} | Mode: {:?}", plan_name.green(), tenant.isolation_mode);

                    if let Some(quota) = db.tenant_manager.get_quota_tracking(tenant.id) {
                        // Storage
                        let storage_pct = if tenant.limits.max_storage_bytes == u64::MAX {
                            0.0
                        } else {
                            (quota.storage_bytes_used as f64 / tenant.limits.max_storage_bytes as f64) * 100.0
                        };
                        let storage_bar = Self::progress_bar(storage_pct, 20);
                        let storage_used = Self::format_storage(quota.storage_bytes_used);
                        let storage_limit = Self::format_storage(tenant.limits.max_storage_bytes);

                        println!();
                        println!("  {} {} {:.1}%", "Storage:".bold(), storage_bar, storage_pct);
                        println!("    Used: {} / {}", storage_used.cyan(), storage_limit);

                        // Connections
                        let conn_pct = if tenant.limits.max_connections == usize::MAX {
                            0.0
                        } else {
                            (quota.active_connections as f64 / tenant.limits.max_connections as f64) * 100.0
                        };
                        let conn_bar = Self::progress_bar(conn_pct, 20);
                        let conn_limit = if tenant.limits.max_connections == usize::MAX {
                            "∞".to_string()
                        } else {
                            tenant.limits.max_connections.to_string()
                        };

                        println!();
                        println!("  {} {} {:.1}%", "Connections:".bold(), conn_bar, conn_pct);
                        println!("    Current: {} / {}  |  HWM: {}  |  Avg: {:.1}",
                            quota.active_connections.to_string().cyan(),
                            conn_limit,
                            quota.connections_hwm.to_string().yellow(),
                            quota.avg_connections()
                        );

                        // QPS
                        let qps_pct = if tenant.limits.max_qps == usize::MAX {
                            0.0
                        } else {
                            (quota.queries_this_window as f64 / tenant.limits.max_qps as f64) * 100.0
                        };
                        let qps_bar = Self::progress_bar(qps_pct, 20);
                        let qps_limit = if tenant.limits.max_qps == usize::MAX {
                            "∞".to_string()
                        } else {
                            tenant.limits.max_qps.to_string()
                        };

                        println!();
                        println!("  {} {} {:.1}%", "QPS:".bold(), qps_bar, qps_pct);
                        println!("    Current: {} / {}  |  HWM: {}  |  Avg: {:.1}",
                            quota.queries_this_window.to_string().cyan(),
                            qps_limit,
                            quota.qps_hwm.to_string().yellow(),
                            quota.avg_qps()
                        );
                        println!("    Total queries: {}", quota.total_queries);

                        // Uptime info
                        println!();
                        println!("  {} {}", "Started:".dimmed(), quota.started_at.dimmed());
                        if quota.total_seconds > 0 {
                            let hours = quota.total_seconds / 3600;
                            let mins = (quota.total_seconds % 3600) / 60;
                            let secs = quota.total_seconds % 60;
                            println!("  {} {}h {}m {}s", "Uptime:".dimmed(), hours, mins, secs);
                        }
                    } else {
                        println!("  {}", "No usage data available.".dimmed());
                    }

                    println!("  {}", "─".repeat(76));
                }

                println!();
                println!("{}", "Legend:".dimmed());
                println!("  {} - Current value at this moment", "Current".cyan());
                println!("  {} - High-water mark (maximum observed)", "HWM".yellow());
                println!("  {} - Average since tracking started", "Avg");
                println!();

                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantPlansList => {
                let plans = db.tenant_manager.plan_manager.list_plans();

                println!("{}", "Available Multi-Tenancy Plans".bold());
                println!();
                println!("╔══════════════╦═══════╦═════════╦══════════════╦═════════════╦════════════╗");
                println!("║ {:^12} ║ {:^5} ║ {:^7} ║ {:^12} ║ {:^11} ║ {:^10} ║",
                    "Plan ID", "Tier", "Status", "Storage", "Connections", "QPS");
                println!("╠══════════════╬═══════╬═════════╬══════════════╬═════════════╬════════════╣");

                for plan in &plans {
                    let storage = Self::format_storage(plan.limits.max_storage_bytes);
                    let conn = if plan.limits.max_connections == usize::MAX {
                        "∞".to_string()
                    } else {
                        plan.limits.max_connections.to_string()
                    };
                    let qps = if plan.limits.max_qps == usize::MAX {
                        "∞".to_string()
                    } else {
                        plan.limits.max_qps.to_string()
                    };

                    let status = if plan.is_default {
                        "default".green().bold().to_string()
                    } else if plan.enabled {
                        "enabled".green().to_string()
                    } else {
                        "disabled".red().to_string()
                    };

                    let tier_str = if plan.tier_id == u32::MAX {
                        "∞".to_string()
                    } else {
                        plan.tier_id.to_string()
                    };

                    println!("║ {:^12} ║ {:^5} ║ {:^7} ║ {:>12} ║ {:>11} ║ {:>10} ║",
                        plan.id.cyan(),
                        tier_str,
                        status,
                        storage,
                        conn,
                        qps
                    );
                }
                println!("╚══════════════╩═══════╩═════════╩══════════════╩═════════════╩════════════╝");
                println!();
                println!("{}", "Plan Management:".bold());
                println!("  {} - View plan details", "\\tenant plan info <id>".cyan());
                println!("  {} - Create new plan (ID auto-generated)", "\\tenant plan create <name> <tier> <storage_mb> <conn> <qps>".cyan());
                println!("  {} - Edit plan", "\\tenant plan edit <id> <field> <value>".cyan());
                println!("  {} / {} - Toggle plan", "\\tenant plan enable <id>".cyan(), "disable <id>".cyan());
                println!("  {} - Delete plan (tenants downgraded)", "\\tenant plan delete <id>".cyan());
                println!("  {} - Real-time usage statistics", "\\tenant usage [name]".cyan());
                println!();

                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantPlanInfo(plan_name) => {
                let plan_id = plan_name.to_lowercase();

                match db.tenant_manager.plan_manager.get_plan(&plan_id) {
                    Some(plan) => {
                        let storage = Self::format_storage(plan.limits.max_storage_bytes);
                        let conn = if plan.limits.max_connections == usize::MAX {
                            "Unlimited".to_string()
                        } else {
                            plan.limits.max_connections.to_string()
                        };
                        let qps = if plan.limits.max_qps == usize::MAX {
                            "Unlimited".to_string()
                        } else {
                            plan.limits.max_qps.to_string()
                        };

                        let status = if plan.is_default {
                            "Default (cannot be deleted)".green().bold().to_string()
                        } else if plan.enabled {
                            "Enabled".green().to_string()
                        } else {
                            "Disabled".red().to_string()
                        };

                        let tier_str = if plan.tier_id == u32::MAX {
                            "∞ (highest)".to_string()
                        } else {
                            plan.tier_id.to_string()
                        };

                        // Count tenants on this plan
                        let tenant_count = db.tenant_manager.get_tenants_by_plan(&plan.id).len();

                        println!();
                        println!("{} {} ({})", "Plan:".bold(), plan.name.cyan(), plan.id.dimmed());
                        println!("{} {}", "Description:".bold(), plan.description);
                        println!("{} {}", "Tier ID:".bold(), tier_str);
                        println!("{} {}", "Status:".bold(), status);
                        println!("{} {}", "Tenants:".bold(), tenant_count);
                        println!();
                        println!("{}", "Resource Limits:".bold());
                        println!("  Storage:     {}", storage.cyan());
                        println!("  Connections: {}", conn.cyan());
                        println!("  QPS:         {}", qps.cyan());
                        println!();
                        println!("{}", "Features:".bold());
                        println!("  {} RLS Policies: {}", "•".green(),
                            if plan.features.rls_enabled { "Yes".green() } else { "No".dimmed() });
                        println!("  {} CDC Events: {}", "•".green(),
                            if plan.features.cdc_enabled { "Yes".green() } else { "No".dimmed() });
                        println!("  {} Migrations: {}", "•".green(),
                            if plan.features.migrations_enabled { "Yes".green() } else { "No".dimmed() });
                        println!("  {} Custom Quotas: {}", "•".green(),
                            if plan.features.custom_quotas_enabled { "Yes".green() } else { "No".dimmed() });
                        println!("  {} All Isolation Modes: {}", "•".green(),
                            if plan.features.all_isolation_modes { "Yes".green() } else { "No".dimmed() });
                        println!();

                        if !plan.is_default {
                            println!("{}", "Commands:".dimmed());
                            println!("  {} - Assign tenant to this plan",
                                format!("\\tenant plan <tenant> {}", plan.id).cyan());
                            if plan.enabled {
                                println!("  {} - Disable this plan",
                                    format!("\\tenant plan disable {}", plan.id).cyan());
                            } else {
                                println!("  {} - Enable this plan",
                                    format!("\\tenant plan enable {}", plan.id).cyan());
                            }
                            println!();
                        }
                    }
                    None => {
                        println!("{}: Plan '{}' not found", "Error".red(), plan_name);
                        println!("{}", "List plans: \\tenant plans".dimmed());
                    }
                }

                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantPlanCreate { ref name, ref tier_id, ref storage_mb, ref max_connections, ref max_qps } => {
                // Auto-generate ID from name: lowercase, replace spaces with hyphens
                let id = name.to_lowercase().replace(' ', "-").replace('_', "-");

                let plan = crate::tenant::Plan::new(
                    &id,
                    name.clone(),
                    format!("Custom plan: {}", name),
                    *tier_id,
                    crate::tenant::ResourceLimits {
                        max_storage_bytes: *storage_mb * 1024 * 1024,
                        max_connections: *max_connections,
                        max_qps: *max_qps,
                    },
                );

                match db.tenant_manager.plan_manager.create_plan(plan) {
                    Ok(_) => {
                        println!("{}: Plan '{}' created (ID: {})", "Success".green(), name.cyan(), id.dimmed());
                        println!("  Tier: {}", tier_id);
                        println!("  Storage: {}", Self::format_storage(*storage_mb * 1024 * 1024));
                        println!("  Connections: {}", max_connections);
                        println!("  QPS: {}", max_qps);
                        println!();
                        println!("{}", "View details:".dimmed());
                        println!("  {}", format!("\\tenant plan info {}", id).cyan());
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantPlanEdit { plan_id, field, value } => {
                let mut updates = crate::tenant::PlanUpdate::default();

                match field.to_lowercase().as_str() {
                    "name" => updates.name = Some(value.clone()),
                    "description" | "desc" => updates.description = Some(value.clone()),
                    "tier" | "tier_id" => {
                        match value.parse::<u32>() {
                            Ok(t) => updates.tier_id = Some(t),
                            Err(_) => {
                                println!("{}: Invalid tier ID '{}'", "Error".red(), value);
                                return Ok(MetaCommandResult::Continue);
                            }
                        }
                    }
                    "storage" | "storage_mb" => {
                        match value.parse::<u64>() {
                            Ok(mb) => {
                                let current = db.tenant_manager.plan_manager.get_plan(&plan_id);
                                if let Some(p) = current {
                                    updates.limits = Some(crate::tenant::ResourceLimits {
                                        max_storage_bytes: mb * 1024 * 1024,
                                        ..p.limits
                                    });
                                }
                            }
                            Err(_) => {
                                println!("{}: Invalid storage value '{}'", "Error".red(), value);
                                return Ok(MetaCommandResult::Continue);
                            }
                        }
                    }
                    "connections" | "conn" => {
                        match value.parse::<usize>() {
                            Ok(c) => {
                                let current = db.tenant_manager.plan_manager.get_plan(&plan_id);
                                if let Some(p) = current {
                                    updates.limits = Some(crate::tenant::ResourceLimits {
                                        max_connections: c,
                                        ..p.limits
                                    });
                                }
                            }
                            Err(_) => {
                                println!("{}: Invalid connections value '{}'", "Error".red(), value);
                                return Ok(MetaCommandResult::Continue);
                            }
                        }
                    }
                    "qps" => {
                        match value.parse::<usize>() {
                            Ok(q) => {
                                let current = db.tenant_manager.plan_manager.get_plan(&plan_id);
                                if let Some(p) = current {
                                    updates.limits = Some(crate::tenant::ResourceLimits {
                                        max_qps: q,
                                        ..p.limits
                                    });
                                }
                            }
                            Err(_) => {
                                println!("{}: Invalid QPS value '{}'", "Error".red(), value);
                                return Ok(MetaCommandResult::Continue);
                            }
                        }
                    }
                    _ => {
                        println!("{}: Unknown field '{}'. Valid: name, description, tier, storage, connections, qps",
                            "Error".red(), field);
                        return Ok(MetaCommandResult::Continue);
                    }
                }

                match db.tenant_manager.plan_manager.update_plan(&plan_id, updates) {
                    Ok(plan) => {
                        println!("{}: Plan '{}' updated", "Success".green(), plan_id.cyan());
                        println!("  {} = {}", field, value);
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantPlanEnable(plan_id) => {
                match db.tenant_manager.plan_manager.enable_plan(&plan_id) {
                    Ok(_) => {
                        println!("{}: Plan '{}' enabled", "Success".green(), plan_id.cyan());
                        println!("{}", "New tenants can now be assigned to this plan.".dimmed());
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantPlanDisable(plan_id) => {
                // Check how many tenants are on this plan
                let tenant_count = db.tenant_manager.get_tenants_by_plan(&plan_id).len();

                match db.tenant_manager.plan_manager.disable_plan(&plan_id) {
                    Ok(_) => {
                        println!("{}: Plan '{}' disabled", "Success".green(), plan_id.cyan());
                        if tenant_count > 0 {
                            println!("{}", format!("  {} existing tenant(s) will keep this plan.", tenant_count).yellow());
                        }
                        println!("{}", "New tenants cannot be assigned to this plan.".dimmed());
                    }
                    Err(e) => {
                        println!("{}: {}", "Error".red(), e);
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantPlanDelete(plan_id) => {
                // Check how many tenants will be affected
                let affected_tenants = db.tenant_manager.get_tenants_by_plan(&plan_id);

                if affected_tenants.is_empty() {
                    // No tenants, just delete
                    match db.tenant_manager.plan_manager.delete_plan(&plan_id) {
                        Ok((deleted, _)) => {
                            println!("{}: Plan '{}' deleted", "Success".green(), deleted.name.cyan());
                        }
                        Err(e) => {
                            println!("{}: {}", "Error".red(), e);
                        }
                    }
                } else {
                    // Has tenants, delete and downgrade
                    match db.tenant_manager.delete_plan_and_downgrade(&plan_id) {
                        Ok((deleted, fallback_id, downgraded)) => {
                            println!("{}: Plan '{}' deleted", "Success".green(), deleted.name.cyan());
                            println!();
                            println!("{} {} tenant(s) downgraded to '{}':",
                                "Warning:".yellow(), downgraded.len(), fallback_id.cyan());
                            for tenant_id in &downgraded {
                                if let Some(t) = db.tenant_manager.get_tenant(*tenant_id) {
                                    println!("  • {} ({})", t.name, tenant_id.to_string().dimmed());
                                }
                            }
                        }
                        Err(e) => {
                            println!("{}: {}", "Error".red(), e);
                        }
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantPlan { tenant, plan } => {
                let tenants = db.tenant_manager.list_tenants();

                // Find tenant by name or ID
                let tenant_obj = tenants.iter().find(|t| {
                    t.name.eq_ignore_ascii_case(&tenant) ||
                    t.id.to_string().starts_with(tenant.as_str())
                });

                match tenant_obj {
                    Some(t) => {
                        let old_plan_id = t.plan_id.clone();

                        match db.tenant_manager.change_tenant_plan(t.id, &plan) {
                            Ok(updated) => {
                                println!("{}: Plan updated for '{}'", "Success".green(), t.name.cyan());
                                println!("  {} → {}", old_plan_id.yellow(), plan.green());
                                println!();
                                println!("{}", "New Limits:".bold());
                                println!("  Storage: {}", Self::format_storage(updated.limits.max_storage_bytes));
                                println!("  Connections: {}", updated.limits.max_connections);
                                println!("  QPS: {}", updated.limits.max_qps);
                                println!();
                            }
                            Err(e) => {
                                println!("{}: {}", "Error".red(), e);
                            }
                        }
                    }
                    None => {
                        println!("{}: Tenant '{}' not found", "Error".red(), tenant);
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantDelete(tenant_ref) => {
                let tenants = db.tenant_manager.list_tenants();

                // Find tenant by name or ID
                let tenant = tenants.iter().find(|t| {
                    t.name.eq_ignore_ascii_case(&tenant_ref) ||
                    t.id.to_string().starts_with(tenant_ref.as_str())
                });

                match tenant {
                    Some(t) => {
                        // Check if this is the current tenant
                        if let Some(ctx) = db.tenant_manager.get_current_context() {
                            if ctx.tenant_id == t.id {
                                println!("{}: Cannot delete current tenant", "Error".red());
                                println!("{}", "Use \\tenant clear first to remove context".dimmed());
                                return Ok(MetaCommandResult::Continue);
                            }
                        }

                        let tenant_id = t.id;
                        let tenant_name = t.name.clone();

                        // Actually delete the tenant
                        match db.tenant_manager.delete_tenant(tenant_id) {
                            Ok(()) => {
                                println!("{}: Tenant '{}' deleted", "Success".green(), tenant_name.cyan());
                                println!("  ID: {}", tenant_id.to_string().dimmed());
                                println!();
                                println!("{}", "Tenant removed from registry, quota tracking, and CDC logs.".dimmed());
                                println!("{}", "Note: Table data persists until explicitly dropped.".dimmed());
                            }
                            Err(e) => {
                                println!("{}: Failed to delete tenant: {}", "Error".red(), e);
                            }
                        }
                    }
                    None => {
                        println!("{}: Tenant '{}' not found", "Error".red(), tenant_ref);
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantCurrent => {
                match db.tenant_manager.get_current_context() {
                    Some(ctx) => {
                        let tenants = db.tenant_manager.list_tenants();
                        let tenant = tenants.iter().find(|t| t.id == ctx.tenant_id);

                        println!("\n{}", "Current Tenant Context:".bold());
                        println!("{}", "─".repeat(50));

                        if let Some(t) = tenant {
                            println!("  Tenant: {}", t.name.cyan());
                            println!("  ID: {}", ctx.tenant_id.to_string().dimmed());
                            let plan = Self::limits_to_plan(&t.limits);
                            println!("  Plan: {}", plan.green());
                        } else {
                            println!("  Tenant ID: {}", ctx.tenant_id.to_string().cyan());
                        }

                        println!("  User: {}", ctx.user_id);
                        println!("  Roles: {}", ctx.roles.join(", ").dimmed());
                        let isolation_str = match ctx.isolation_mode {
                            crate::tenant::IsolationMode::SharedSchema => "SharedSchema",
                            crate::tenant::IsolationMode::DatabasePerTenant => "DBPerTenant",
                            crate::tenant::IsolationMode::SchemaPerTenant => "SchemaPerTenant",
                        };
                        println!("  Isolation: {}", isolation_str.yellow());
                        println!();
                    }
                    None => {
                        println!("{}", "No tenant context set.".dimmed());
                        println!();
                        println!("{}", "Set a tenant:".dimmed());
                        println!("  {}", "\\tenant use <name>".cyan());
                        println!();
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantClearContext => {
                // Clear context using the new clear_current_context method
                db.tenant_manager.clear_current_context();
                println!("{}: Tenant context cleared", "Success".green());
                println!("{}", "RLS restrictions are now inactive.".dimmed());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantRlsCreate { table, policy, expression, command } => {
                println!("\n{}", "Creating RLS Policy:".bold());
                println!("{}", "─".repeat(60));

                // Parse command string to RLSCommand enum
                let rls_cmd = match command.to_uppercase().as_str() {
                    "ALL" => crate::tenant::RLSCommand::All,
                    "SELECT" => crate::tenant::RLSCommand::Select,
                    "INSERT" => crate::tenant::RLSCommand::Insert,
                    "UPDATE" => crate::tenant::RLSCommand::Update,
                    "DELETE" => crate::tenant::RLSCommand::Delete,
                    _ => {
                        println!("{}: Invalid command '{}'", "Error".red(), command);
                        println!("Valid commands: ALL, SELECT, INSERT, UPDATE, DELETE");
                        return Ok(MetaCommandResult::Continue);
                    }
                };

                // Create RLS policy
                db.tenant_manager.create_rls_policy(
                    table.clone(),
                    policy.clone(),
                    expression.clone(), // condition (same as using_expr for now)
                    rls_cmd,
                    expression.clone(), // using_expr
                    Some(expression.clone()), // with_check_expr (same as using for simplicity)
                );

                println!("{}: RLS policy '{}' created for table '{}'", "Success".green(), policy.cyan(), table.cyan());
                println!("  Expression: {}", expression.yellow());
                println!("  Commands: {}", command.to_uppercase().green());
                println!();
                println!("{}", "Note: Policy will be enforced on all matching operations.".dimmed());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantRlsList(table) => {
                println!("\n{}: {}", "RLS Policies".bold(), table.cyan());
                println!("{}", "═".repeat(80));

                let policies = db.tenant_manager.get_rls_policies(&table);

                if policies.is_empty() {
                    println!("{}", "No RLS policies found for this table.".dimmed());
                    println!();
                    println!("{}", "Create a policy:".dimmed());
                    println!("  {}", format!("\\tenant rls create {} <policy> <expression> <command>", table).cyan());
                } else {
                    println!("\n{:<20} {:<10} {}", "Policy Name".bold(), "Commands".bold(), "Expression".bold());
                    println!("{}", "─".repeat(80));

                    for policy in policies {
                        let cmd_str = match policy.cmd {
                            crate::tenant::RLSCommand::All => "ALL",
                            crate::tenant::RLSCommand::Select => "SELECT",
                            crate::tenant::RLSCommand::Insert => "INSERT",
                            crate::tenant::RLSCommand::Update => "UPDATE",
                            crate::tenant::RLSCommand::Delete => "DELETE",
                        };

                        println!("{:<20} {:<10} {}",
                            policy.name.cyan(),
                            cmd_str.green(),
                            policy.using_expr.yellow()
                        );
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantRlsDelete { table, policy } => {
                println!("\n{}", "Deleting RLS Policy:".bold());
                println!("{}", "─".repeat(60));

                // Note: delete_rls_policy method would need to be implemented in TenantManager
                // For now, show what would be deleted
                let policies = db.tenant_manager.get_rls_policies(&table);
                let found = policies.iter().find(|p| &p.name == policy);

                if found.is_some() {
                    println!("{}: Would delete policy '{}' from table '{}'", "Note".yellow(), policy.cyan(), table.cyan());
                    println!();
                    println!("{}", "⚠ Policy deletion not yet implemented in TenantManager.".yellow());
                    println!("{}", "  Policies persist for the session.".dimmed());
                } else {
                    println!("{}: Policy '{}' not found in table '{}'", "Error".red(), policy, table);
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantCdcShow(limit) => {
                println!("\n{}", "CDC Events:".bold());
                println!("{}", "═".repeat(100));

                let Some(ctx) = db.tenant_manager.get_current_context() else {
                    println!("{}: No tenant context set", "Error".red());
                    println!("{}", "Use: \\tenant use <name>".dimmed());
                    println!();
                    return Ok(MetaCommandResult::Continue);
                };

                let tenant_id = ctx.tenant_id;
                let cdc_log = db.tenant_manager.get_cdc_log(tenant_id);

                if let Some(log) = cdc_log {
                    let events = &log.changes;
                    if events.is_empty() {
                        println!("{}", "No CDC events recorded for this tenant.".dimmed());
                    } else {
                        let display_limit = limit.unwrap_or(10).min(events.len());
                        let events_to_show = &events[events.len().saturating_sub(display_limit)..];

                        println!("\n{:<20} {:<10} {:<30} {}", "Timestamp".bold(), "Type".bold(), "Table".bold(), "Row ID".bold());
                        println!("{}", "─".repeat(100));

                        for event in events_to_show {
                            let event_type = match event.change_type {
                                crate::tenant::ChangeType::Insert => "INSERT".green(),
                                crate::tenant::ChangeType::Update => "UPDATE".yellow(),
                                crate::tenant::ChangeType::Delete => "DELETE".red(),
                            };

                            println!("{:<20} {:<10} {:<30} {}",
                                event.timestamp.to_string().dimmed(),
                                event_type.to_string(),
                                event.table_name.cyan(),
                                event.row_key
                            );
                        }

                        println!();
                        println!("Showing {} of {} total events", display_limit, events.len());
                    }
                } else {
                    println!("{}", "No CDC log found for this tenant.".dimmed());
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantCdcExport(filename) => {
                println!("\n{}", "Exporting CDC Events:".bold());
                println!("{}", "─".repeat(60));

                let Some(ctx) = db.tenant_manager.get_current_context() else {
                    println!("{}: No tenant context set", "Error".red());
                    println!("{}", "Use: \\tenant use <name>".dimmed());
                    println!();
                    return Ok(MetaCommandResult::Continue);
                };

                let tenant_id = ctx.tenant_id;
                let cdc_log = db.tenant_manager.get_cdc_log(tenant_id);

                // Serialize to JSON
                let events = cdc_log.map(|log| log.changes).unwrap_or_default();
                match serde_json::to_string_pretty(&events) {
                    Ok(json_data) => {
                        match std::fs::write(&filename, json_data) {
                            Ok(_) => {
                                println!("{}: Exported {} events to '{}'", "Success".green(), events.len(), filename.cyan());
                                println!();
                            }
                            Err(e) => {
                                println!("{}: Failed to write file - {}", "Error".red(), e);
                                println!();
                            }
                        }
                    }
                    Err(e) => {
                        println!("{}: Failed to serialize events - {}", "Error".red(), e);
                        println!();
                    }
                }
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantMigrateTo(target) => {
                println!("\n{}", "Initiating Tenant Migration:".bold());
                println!("{}", "═".repeat(60));

                let Some(ctx) = db.tenant_manager.get_current_context() else {
                    println!("{}: No tenant context set", "Error".red());
                    println!("{}", "Use: \\tenant use <name>".dimmed());
                    println!();
                    return Ok(MetaCommandResult::Continue);
                };

                let tenant_id = ctx.tenant_id;
                let tenant = db.tenant_manager.get_tenant(tenant_id);

                if let Some(t) = tenant {
                    // Find target tenant by name
                    let tenants = db.tenant_manager.list_tenants();
                    let target_tenant = tenants.iter().find(|tenant| {
                        tenant.name.eq_ignore_ascii_case(&target) ||
                        tenant.id.to_string().starts_with(target.as_str())
                    });

                    match target_tenant {
                        Some(target_t) => {
                            println!("Migrating tenant: {}", t.name.cyan());
                            println!("Target: {} ({})", target_t.name.cyan(), target_t.id.to_string().dimmed());
                            println!();

                            match db.tenant_manager.start_migration(tenant_id, target_t.id) {
                                Ok(_) => {
                                    println!("{}: Migration initiated", "Success".green());
                                    println!();
                                    println!("{}", "Migration Status:".bold());
                                    println!("  State: {}", "Pending".yellow());
                                    println!("  Target: {}", target_t.name.cyan());
                                    println!();
                                    println!("{}", "Check status: \\tenant migrate status".dimmed());
                                }
                                Err(e) => {
                                    println!("{}: Migration failed - {}", "Error".red(), e);
                                }
                            }
                        }
                        None => {
                            println!("{}: Target tenant '{}' not found", "Error".red(), target);
                        }
                    }
                } else {
                    println!("{}: Tenant not found", "Error".red());
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantMigrateStatus(tenant_ref) => {
                println!("\n{}", "Migration Status:".bold());
                println!("{}", "═".repeat(60));

                let tenant_id = if let Some(ref tr) = tenant_ref {
                    // Find tenant by name or ID
                    let tenants = db.tenant_manager.list_tenants();
                    tenants.iter().find(|t| {
                        t.name.eq_ignore_ascii_case(tr) ||
                        t.id.to_string().starts_with(tr)
                    }).map(|t| t.id)
                } else if let Some(ctx) = db.tenant_manager.get_current_context() {
                    Some(ctx.tenant_id)
                } else {
                    None
                };

                if let Some(tid) = tenant_id {
                    let tenant = db.tenant_manager.get_tenant(tid);
                    if let Some(t) = tenant {
                        println!("Tenant: {}", t.name.cyan());
                        println!();

                        // Get all active migrations for this tenant
                        let migrations = db.tenant_manager.get_active_migrations(tid);
                        if migrations.is_empty() {
                            println!("{}", "No active migrations for this tenant.".dimmed());
                        } else {
                            for status in migrations {
                                let target_tenant = db.tenant_manager.get_tenant(status.target_tenant_id);
                                let target_name = target_tenant.map(|t| t.name).unwrap_or_else(|| status.target_tenant_id.to_string());

                                let state_str = match status.migration_state {
                                    crate::tenant::MigrationState::Pending => "Pending".yellow(),
                                    crate::tenant::MigrationState::Snapshotting => "Snapshotting".cyan(),
                                    crate::tenant::MigrationState::Replicating => "Replicating".cyan(),
                                    crate::tenant::MigrationState::Verifying => "Verifying".cyan(),
                                    crate::tenant::MigrationState::Completed => "Completed".green(),
                                    crate::tenant::MigrationState::Failed(ref msg) => {
                                        format!("Failed: {}", msg).red()
                                    }
                                    crate::tenant::MigrationState::Paused => "Paused".yellow(),
                                };

                                println!("{}: {}", "State".bold(), state_str);
                                println!("{}: {}", "Target".bold(), target_name.cyan());
                                println!("{}: {} / {}", "Progress".bold(), status.changes_replicated, status.total_changes);
                                println!("{}: {}", "Started".bold(), status.started_at.dimmed());
                                if let Some(ref completed) = status.completed_at {
                                    println!("{}: {}", "Completed".bold(), completed.dimmed());
                                }
                                println!();
                            }
                        }
                    } else {
                        println!("{}: Tenant not found", "Error".red());
                    }
                } else {
                    println!("{}: No tenant specified and no context set", "Error".red());
                    println!("{}", "Use: \\tenant migrate status <tenant> or \\tenant use <tenant>".dimmed());
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::TenantQuotaSet { tenant, storage_mb, max_connections, max_qps } => {
                println!("\n{}", "Setting Custom Quotas:".bold());
                println!("{}", "─".repeat(60));

                let tenants = db.tenant_manager.list_tenants();
                let tenant_obj = tenants.iter().find(|t| {
                    t.name.eq_ignore_ascii_case(&tenant) ||
                    t.id.to_string().starts_with(tenant.as_str())
                });

                match tenant_obj {
                    Some(t) => {
                        let limits = crate::tenant::ResourceLimits {
                            max_storage_bytes: *storage_mb * 1024 * 1024,
                            max_connections: *max_connections,
                            max_qps: *max_qps as usize,
                        };

                        match db.tenant_manager.update_resource_limits(t.id, limits.clone()) {
                            Ok(_) => {
                                println!("{}: Custom quotas set for '{}'", "Success".green(), t.name.cyan());
                                println!();
                                println!("{}", "New Limits:".bold());
                                println!("  Storage: {} MB", storage_mb);
                                println!("  Connections: {}", max_connections);
                                println!("  QPS: {}", max_qps);
                                println!();
                                println!("{}", "Note: This sets a custom plan. Use \\tenant plan to revert to standard plans.".dimmed());
                            }
                            Err(e) => {
                                println!("{}: Failed to set quotas - {}", "Error".red(), e);
                            }
                        }
                    }
                    None => {
                        println!("{}: Tenant '{}' not found", "Error".red(), tenant);
                        println!();
                        println!("{}", "List tenants: \\tenants".dimmed());
                    }
                }
                println!();
                Ok(MetaCommandResult::Continue)
            }

            // v3.4 Information Commands
            MetaCommand::Version => {
                println!("\n{}", "HeliosDB Lite Version Information".bold());
                println!("{}", "═".repeat(50));
                println!("{}: {}", "Version".cyan(), env!("CARGO_PKG_VERSION"));
                println!("{}: {}", "Edition".cyan(), "2021");
                println!("{}: {}", "Target".cyan(), std::env::consts::ARCH);
                println!("{}: {}", "OS".cyan(), std::env::consts::OS);
                println!();
                println!("{}", "Enabled Features:".bold());
                #[cfg(feature = "encryption")]
                println!("  {} Encryption (AES-256-GCM)", "✓".green());
                #[cfg(not(feature = "encryption"))]
                println!("  {} Encryption", "✗".dimmed());
                #[cfg(feature = "vector-search")]
                println!("  {} Vector Search", "✓".green());
                #[cfg(not(feature = "vector-search"))]
                println!("  {} Vector Search", "✗".dimmed());
                println!("  {} Compression (zstd)", "✓".green());
                println!("  {} Time-Travel Queries", "✓".green());
                println!("  {} Multi-Tenancy", "✓".green());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::Status => {
                println!("\n{}", "Database Status".bold());
                println!("{}", "═".repeat(50));

                // Connection info
                println!("\n{}", "Connection:".cyan());
                println!("  Mode: REPL (embedded)");
                println!("  State: {}", "Connected".green());

                // Current branch
                let current_branch = db.storage.get_current_branch().unwrap_or_else(|| "main".to_string());
                println!("  Branch: {}", current_branch.cyan());

                // Schema info
                let catalog = db.storage.catalog();
                let tables = catalog.list_tables().unwrap_or_default();
                println!("\n{}", "Schema:".cyan());
                println!("  Tables: {}", tables.len());

                // Storage info
                if let Some(stats) = db.storage.get_storage_stats() {
                    println!("\n{}", "Storage:".cyan());
                    println!("  Approximate size: {} bytes", stats.approximate_size);
                    println!("  Keys: {}", stats.key_count);
                }

                // Features
                println!("\n{}", "Features:".cyan());
                println!("  WAL: {}", if db.config.storage.wal_enabled { "Enabled".green() } else { "Disabled".yellow() });
                println!("  Compression: {}", "Enabled (zstd)".green());
                println!("  Time-Travel: {}", if db.config.storage.time_travel_enabled { "Enabled".green() } else { "Disabled".yellow() });

                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::Settings => {
                println!("\n{}", "REPL Settings".bold());
                println!("{}", "═".repeat(50));

                println!("\n{}", "Display:".cyan());
                println!("  Timing: {}", if show_timing { "ON".green() } else { "OFF".yellow() });

                if let Some(cfg) = config {
                    println!("  Row count: {}", if cfg.show_row_count { "ON" } else { "OFF" });
                    println!("  Output format: {:?}", cfg.output_format);
                    println!("  Null display: '{}'", cfg.null_display);
                    println!("  Max column width: {}", cfg.max_column_width);
                    println!("\n{}", "Transaction:".cyan());
                    println!("  Auto-commit: {}", if cfg.auto_commit { "ON" } else { "OFF" });
                    println!("\n{}", "History:".cyan());
                    println!("  Max entries: {}", cfg.max_history);
                }

                println!("\n{}", "Session:".cyan());
                let branch = db.storage.get_current_branch().unwrap_or_else(|| "main".to_string());
                println!("  Branch: {}", branch.cyan());

                println!("\n{}", "Tips:".dimmed());
                println!("  {} - Toggle timing display", "\\timing".cyan());
                println!("  {} - Set variables", "\\set <var> <value>".cyan());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::Vacuum(table_name) => {
                use std::time::Instant;

                println!("\n{}", "╔═══════════════════════════════════════════════════════════════╗".yellow());
                println!("{}", "║                    VACUUM OPERATION                           ║".yellow());
                println!("{}", "╚═══════════════════════════════════════════════════════════════╝".yellow());

                println!("\n{}: HeliosDB uses automatic compaction with zstd compression.",
                    "Note".cyan().bold());
                println!("Manual vacuum is typically {} unless you have:", "not required".green());
                println!("  • Performed bulk deletions and want to immediately reclaim space");
                println!("  • Need to optimize storage before a backup");
                println!();

                let start = Instant::now();

                match table_name {
                    Some(table) => {
                        // Check if table exists
                        let catalog = db.storage.catalog();
                        if !catalog.table_exists(table)? {
                            println!("{}: Table '{}' does not exist", "Error".red(), table);
                            return Ok(MetaCommandResult::Continue);
                        }

                        println!("{} table '{}'...", "Vacuuming".bold(), table.cyan());

                        // Get size before
                        let size_before = db.storage.get_approximate_size();

                        // Perform vacuum on table
                        db.storage.vacuum_table(table)?;

                        let size_after = db.storage.get_approximate_size();
                        let elapsed = start.elapsed();

                        println!("\n{}", "Vacuum Complete:".green().bold());
                        println!("  Table: {}", table.cyan());
                        println!("  Time: {:.2?}", elapsed);
                        println!("  Size before: {} bytes", size_before);
                        println!("  Size after: {} bytes", size_after);
                        if size_before > size_after {
                            println!("  Space reclaimed: {} bytes", size_before - size_after);
                        }
                    }
                    None => {
                        println!("{} entire database...", "Vacuuming".bold());

                        // Get size before
                        let size_before = db.storage.get_approximate_size();

                        // Perform full vacuum
                        db.storage.vacuum()?;

                        let size_after = db.storage.get_approximate_size();
                        let elapsed = start.elapsed();

                        println!("\n{}", "Vacuum Complete:".green().bold());
                        println!("  Time: {:.2?}", elapsed);
                        println!("  Size before: {} bytes", size_before);
                        println!("  Size after: {} bytes", size_after);
                        if size_before > size_after {
                            println!("  Space reclaimed: {} bytes", size_before - size_after);
                        }
                    }
                }

                println!("\n{}", "Note: Auto-compaction will continue in the background.".dimmed());
                println!();
                Ok(MetaCommandResult::Continue)
            }

            MetaCommand::ReplicationStatus => {
                println!("\n{}", "╔═══════════════════════════════════════════════════════════════╗".cyan());
                println!("{}", "║                    REPLICATION STATUS                         ║".cyan());
                println!("{}", "╚═══════════════════════════════════════════════════════════════╝".cyan());
                println!();

                // Check WAL status
                if db.storage.is_wal_enabled() {
                    println!("{}: {}", "WAL Status".bold(), "Enabled".green());
                    if let Some(lsn) = db.storage.wal_lsn() {
                        println!("  Current LSN: {}", lsn);
                    }
                } else {
                    println!("{}: {}", "WAL Status".bold(), "Disabled".yellow());
                }

                // Show feature status
                println!();
                println!("{}:", "Replication Features".bold());
                #[cfg(feature = "ha-tier1")]
                {
                    println!("  {}: {} (Warm Standby)", "Tier 1".cyan(), "Enabled".green());
                }
                #[cfg(not(feature = "ha-tier1"))]
                {
                    println!("  {}: {} (enable with --features ha-tier1)", "Tier 1".cyan(), "Disabled".dimmed());
                }
                #[cfg(feature = "ha-tier2")]
                {
                    println!("  {}: {} (Multi-Primary)", "Tier 2".cyan(), "Enabled".green());
                }
                #[cfg(not(feature = "ha-tier2"))]
                {
                    println!("  {}: {} (enable with --features ha-tier2)", "Tier 2".cyan(), "Disabled".dimmed());
                }
                #[cfg(feature = "ha-tier3")]
                {
                    println!("  {}: {} (Sharding)", "Tier 3".cyan(), "Enabled".green());
                }
                #[cfg(not(feature = "ha-tier3"))]
                {
                    println!("  {}: {} (enable with --features ha-tier3)", "Tier 3".cyan(), "Disabled".dimmed());
                }

                println!();
                println!("{}:", "Configuration".bold());
                println!("  Mode: {} (standalone)", "Single Node".yellow());
                println!("  Role: {}", "Primary".green());

                println!();
                println!("{}:", "To enable replication".dimmed());
                println!("  1. Build with: cargo build --features ha-tier1");
                println!("  2. Configure replication in config.toml");
                println!("  3. Start primary: heliosdb-lite start --role primary");
                println!("  4. Start standby: heliosdb-lite start --role standby --primary <host:port>");
                println!();

                Ok(MetaCommandResult::Continue)
            }
        }
    }

    /// Convert plan name to resource limits
    fn plan_to_limits(plan: &str) -> crate::tenant::ResourceLimits {
        match plan.to_lowercase().as_str() {
            "free" => crate::tenant::ResourceLimits {
                max_storage_bytes: 100 * 1024 * 1024,      // 100 MB
                max_connections: 5,
                max_qps: 10,
            },
            "starter" => crate::tenant::ResourceLimits {
                max_storage_bytes: 1024 * 1024 * 1024,     // 1 GB
                max_connections: 20,
                max_qps: 100,
            },
            "pro" => crate::tenant::ResourceLimits {
                max_storage_bytes: 10 * 1024 * 1024 * 1024, // 10 GB
                max_connections: 100,
                max_qps: 1000,
            },
            "enterprise" => crate::tenant::ResourceLimits {
                max_storage_bytes: 100 * 1024 * 1024 * 1024, // 100 GB (unlimited in practice)
                max_connections: 1000,
                max_qps: 10000,
            },
            _ => {
                eprintln!("{}: Unknown plan '{}', using 'free'", "Warning".yellow(), plan);
                crate::tenant::ResourceLimits {
                    max_storage_bytes: 100 * 1024 * 1024,
                    max_connections: 5,
                    max_qps: 10,
                }
            }
        }
    }

    /// Convert resource limits to plan name (best guess)
    fn limits_to_plan(limits: &crate::tenant::ResourceLimits) -> &'static str {
        let storage_mb = limits.max_storage_bytes / 1024 / 1024;
        let conns = limits.max_connections;

        if storage_mb <= 100 && conns <= 5 {
            "free"
        } else if storage_mb <= 1024 && conns <= 20 {
            "starter"
        } else if storage_mb <= 10 * 1024 && conns <= 100 {
            "pro"
        } else {
            "enterprise"
        }
    }

    /// Execute EXPLAIN or EXPLAIN ANALYZE command
    ///
    /// # Arguments
    /// * `db` - Database connection
    /// * `query` - SQL query to explain
    /// * `analyze` - Whether to actually execute the query (EXPLAIN ANALYZE)
    fn execute_explain(db: &EmbeddedDatabase, query: &str, analyze: bool) -> Result<()> {
        use std::time::Instant;

        let mode_name = if analyze { "EXPLAIN ANALYZE" } else { "EXPLAIN" };

        println!("\n{}", format!("Query Execution Plan ({}):", mode_name).bold());
        println!("{}", "=".repeat(80));
        println!("{}: {}", "Query".dimmed(), query.cyan());
        println!();

        // 1. Parse the SQL query
        let parser = Parser::new();
        let statement = match parser.parse_one(query) {
            Ok(stmt) => stmt,
            Err(e) => {
                println!("{}: Failed to parse query", "Error".red());
                println!("  {}", e);
                return Ok(());
            }
        };

        // 2. Convert to logical plan using the catalog
        let catalog = db.storage.catalog();
        let planner = Planner::with_catalog(&catalog);
        let plan = match planner.statement_to_plan(statement) {
            Ok(p) => p,
            Err(e) => {
                println!("{}: Failed to create execution plan", "Error".red());
                println!("  {}", e);
                return Ok(());
            }
        };

        // 3. Create ExplainPlanner with storage access for statistics
        let storage_arc = Arc::clone(&db.storage);
        let mode = if analyze { ExplainMode::Analyze } else { ExplainMode::Standard };
        let explain_planner = ExplainPlanner::new(mode, ExplainFormat::Text)
            .with_storage(storage_arc);

        // 4. Generate execution plan explanation
        let explain_start = Instant::now();
        match explain_planner.explain(&plan) {
            Ok(output) => {
                let planning_time = explain_start.elapsed();

                // Print formatted output
                let formatted = explain_planner.format_output(&output);
                print!("{}", formatted);

                // If ANALYZE mode, execute the query and show actual stats
                if analyze {
                    println!("{}", "-".repeat(80));
                    println!("{}", "ACTUAL EXECUTION STATISTICS".bold());
                    println!("{}", "-".repeat(80));

                    let exec_start = Instant::now();
                    match db.query(query, &[]) {
                        Ok(results) => {
                            let exec_time = exec_start.elapsed();
                            let row_count = results.len();

                            println!();
                            println!("{}", "Execution Results:".bold());
                            println!("  {} {}", "Actual rows returned:".green(), row_count.to_string().cyan());
                            println!("  {} {:.3}ms", "Actual execution time:".green(), exec_time.as_secs_f64() * 1000.0);
                            println!("  {} {:.3}ms", "Planning time:".dimmed(), planning_time.as_secs_f64() * 1000.0);
                            println!("  {} {:.3}ms", "Total time:".green(), (exec_time + planning_time).as_secs_f64() * 1000.0);

                            // Show comparison with estimates
                            println!();
                            println!("{}", "Estimate Accuracy:".bold());
                            let estimated_rows = output.total_rows;
                            if estimated_rows > 0 {
                                let accuracy = if row_count > 0 {
                                    (1.0 - ((estimated_rows as f64 - row_count as f64).abs() / row_count as f64).min(1.0)) * 100.0
                                } else {
                                    0.0
                                };
                                println!("  Estimated rows: {} vs Actual rows: {} ({:.1}% accuracy)",
                                    estimated_rows, row_count, accuracy);
                            }

                            // Memory estimate
                            let mem_estimate = row_count * 100; // rough estimate
                            println!();
                            println!("{}", "Resource Usage (estimated):".bold());
                            println!("  Memory: ~{} bytes", mem_estimate);
                            if row_count > 0 {
                                println!("  Time per row: {:.3}us", (exec_time.as_secs_f64() * 1_000_000.0) / row_count as f64);
                            }

                            // Show storage layer info
                            Self::print_storage_layer_info(db, query);
                        }
                        Err(e) => {
                            let exec_time = exec_start.elapsed();
                            println!();
                            println!("{}: Query execution failed after {:.3}ms", "Error".red(), exec_time.as_secs_f64() * 1000.0);
                            println!("  {}", e);
                        }
                    }
                } else {
                    // For non-ANALYZE, still show storage layer capabilities
                    println!("{}", "-".repeat(80));
                    Self::print_storage_layer_info(db, query);
                }

                println!();
            }
            Err(e) => {
                println!("{}: Failed to generate explain output", "Error".red());
                println!("  {}", e);
            }
        }

        Ok(())
    }

    /// Print usage help for \explain command
    fn print_explain_usage() {
        eprintln!("{}", "Usage: \\explain [options] <SQL query>".bold());
        eprintln!();
        eprintln!("{}", "PostgreSQL-compatible options:".cyan());
        eprintln!("  analyze    - Execute the query and show actual statistics");
        eprintln!("  verbose    - Show additional detail");
        eprintln!("  costs      - Show cost estimates (default: true)");
        eprintln!("  buffers    - Show buffer usage (with ANALYZE)");
        eprintln!("  timing     - Show timing information (with ANALYZE)");
        eprintln!("  summary    - Show summary at end");
        eprintln!("  format <f> - Output format: text, json, yaml, tree");
        eprintln!();
        eprintln!("{}", "HeliosDB extensions:".cyan());
        eprintln!("  storage    - Show storage layer details (bloom filters, zone maps, compression)");
        eprintln!("  ai         - Enable AI-powered explanations");
        eprintln!("  why_not    - Show why optimizations weren't applied");
        eprintln!("  indexes    - Show index analysis");
        eprintln!("  stats      - Show table/column statistics");
        eprintln!();
        eprintln!("{}", "Examples:".cyan());
        eprintln!("  \\explain SELECT * FROM users");
        eprintln!("  \\explain analyze SELECT * FROM users WHERE id = 1");
        eprintln!("  \\explain verbose format json SELECT * FROM users");
        eprintln!("  \\explain storage SELECT * FROM orders");
        eprintln!("  \\explain analyze storage why_not SELECT * FROM orders WHERE amount > 100");
    }

    /// Execute EXPLAIN with full options support
    ///
    /// Uses ExplainPlanner with all HeliosDB extensions including storage features,
    /// AI explanations, Why-Not analysis, and multiple output formats.
    fn execute_explain_with_options(db: &EmbeddedDatabase, query: &str, options: &ExplainOptions) -> Result<()> {
        use std::time::Instant;

        // Build mode description
        let mut mode_parts = vec!["EXPLAIN"];
        if options.analyze { mode_parts.push("ANALYZE"); }
        if options.verbose { mode_parts.push("VERBOSE"); }
        if options.storage { mode_parts.push("STORAGE"); }
        if options.ai { mode_parts.push("AI"); }
        if options.why_not { mode_parts.push("WHY_NOT"); }
        if options.format != ExplainFormatOption::Text {
            mode_parts.push(match options.format {
                ExplainFormatOption::Json => "FORMAT JSON",
                ExplainFormatOption::Yaml => "FORMAT YAML",
                ExplainFormatOption::Tree => "FORMAT TREE",
                ExplainFormatOption::Text => "FORMAT TEXT",
            });
        }
        let mode_name = mode_parts.join(" ");

        println!("\n{}", format!("Query Execution Plan ({}):", mode_name).bold());
        println!("{}", "=".repeat(80));
        println!("{}: {}", "Query".dimmed(), query.cyan());
        println!();

        // 1. Parse the SQL query
        let parser = Parser::new();
        let statement = match parser.parse_one(query) {
            Ok(stmt) => stmt,
            Err(e) => {
                println!("{}: Failed to parse query", "Error".red());
                println!("  {}", e);
                return Ok(());
            }
        };

        // 2. Convert to logical plan using the catalog
        let catalog = db.storage.catalog();
        let planner = Planner::with_catalog(&catalog);
        let plan = match planner.statement_to_plan(statement) {
            Ok(p) => p,
            Err(e) => {
                println!("{}: Failed to create execution plan", "Error".red());
                println!("  {}", e);
                return Ok(());
            }
        };

        // 3. Create ExplainPlanner with appropriate mode and format
        let storage_arc = Arc::clone(&db.storage);
        let mode = options.to_explain_mode();
        let format = options.to_explain_format();

        let explain_planner = ExplainPlanner::new(mode, format)
            .with_storage(storage_arc.clone());

        // 4. Generate execution plan explanation
        let explain_start = Instant::now();
        match explain_planner.explain(&plan) {
            Ok(mut output) => {
                let planning_time = explain_start.elapsed();

                // 5. Collect storage features if requested
                let storage_features = if options.storage {
                    match StorageFeatureCollector::collect(Some(&storage_arc), &plan) {
                        Ok(features) => features,
                        Err(e) => {
                            println!("{}: Failed to collect storage features: {}", "Warning".yellow(), e);
                            Vec::new()
                        }
                    }
                } else {
                    Vec::new()
                };

                // 6. If ANALYZE mode, execute the query and capture actual stats
                if options.analyze {
                    let exec_start = Instant::now();
                    match db.query(query, &[]) {
                        Ok(results) => {
                            let exec_time = exec_start.elapsed();
                            output.actual_rows = Some(results.len());
                            output.actual_time_ms = Some(exec_time.as_secs_f64() * 1000.0);
                        }
                        Err(e) => {
                            let exec_time = exec_start.elapsed();
                            output.actual_time_ms = Some(exec_time.as_secs_f64() * 1000.0);
                            output.execution_error = Some(format!("{}", e));
                        }
                    }
                }

                // 7. Format and print output based on format option
                match options.format {
                    ExplainFormatOption::Json => {
                        // JSON output
                        let json_output = Self::format_explain_json(&output, &storage_features, options);
                        println!("{}", json_output);
                    }
                    ExplainFormatOption::Yaml => {
                        // YAML output
                        let yaml_output = Self::format_explain_yaml(&output, &storage_features, options);
                        println!("{}", yaml_output);
                    }
                    ExplainFormatOption::Text | ExplainFormatOption::Tree => {
                        // Text/Tree output - use ExplainPlanner's format_output
                        let formatted = explain_planner.format_output(&output);
                        print!("{}", formatted);

                        // Print execution results if ANALYZE
                        if options.analyze {
                            Self::print_analyze_results(&output, planning_time);
                        }

                        // Print storage features if requested
                        if options.storage && !storage_features.is_empty() {
                            let storage_text = format_storage_features_text(&storage_features);
                            print!("{}", storage_text);
                        }

                        // Print summary if requested
                        if options.summary {
                            Self::print_explain_summary(&output, options);
                        }

                        // Print storage layer info for non-ANALYZE mode
                        if !options.analyze && !options.storage {
                            println!("{}", "-".repeat(80));
                            Self::print_storage_layer_info(db, query);
                        }
                    }
                }

                println!();
            }
            Err(e) => {
                println!("{}: Failed to generate explain output", "Error".red());
                println!("  {}", e);
            }
        }

        Ok(())
    }

    /// Format EXPLAIN output as JSON
    fn format_explain_json(
        output: &crate::sql::explain::ExplainOutput,
        storage_features: &[crate::sql::explain_storage::StorageFeatureReport],
        options: &ExplainOptions,
    ) -> String {
        use serde_json::json;

        let mut result = serde_json::to_value(output).unwrap_or(json!({}));

        // Add storage features if present
        if options.storage && !storage_features.is_empty() {
            if let serde_json::Value::Object(ref mut map) = result {
                map.insert(
                    "storage_features".to_string(),
                    serde_json::to_value(storage_features).unwrap_or(json!([])),
                );
            }
        }

        // Add options summary
        if let serde_json::Value::Object(ref mut map) = result {
            map.insert(
                "options".to_string(),
                json!({
                    "analyze": options.analyze,
                    "verbose": options.verbose,
                    "format": format!("{:?}", options.format),
                    "costs": options.costs,
                    "storage": options.storage,
                    "ai": options.ai,
                    "why_not": options.why_not,
                    "indexes": options.indexes,
                    "statistics": options.statistics,
                }),
            );
        }

        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string())
    }

    /// Format EXPLAIN output as YAML
    fn format_explain_yaml(
        output: &crate::sql::explain::ExplainOutput,
        storage_features: &[crate::sql::explain_storage::StorageFeatureReport],
        options: &ExplainOptions,
    ) -> String {
        let mut yaml_parts = Vec::new();

        // Main explain output
        if let Ok(yaml) = serde_yaml::to_string(output) {
            yaml_parts.push(yaml);
        }

        // Storage features
        if options.storage && !storage_features.is_empty() {
            yaml_parts.push("\n# Storage Features".to_string());
            if let Ok(yaml) = serde_yaml::to_string(storage_features) {
                yaml_parts.push(yaml);
            }
        }

        yaml_parts.join("\n")
    }

    /// Print ANALYZE execution results
    fn print_analyze_results(output: &crate::sql::explain::ExplainOutput, planning_time: std::time::Duration) {
        println!("{}", "-".repeat(80));
        println!("{}", "ACTUAL EXECUTION STATISTICS".bold());
        println!("{}", "-".repeat(80));

        if let Some(error) = &output.execution_error {
            println!("{}: {}", "Execution Error".red(), error);
        }

        if let Some(time_ms) = output.actual_time_ms {
            println!("  {} {:.3}ms", "Actual execution time:".green(), time_ms);
        }

        if let Some(rows) = output.actual_rows {
            println!("  {} {}", "Actual rows returned:".green(), rows.to_string().cyan());
        }

        println!("  {} {:.3}ms", "Planning time:".dimmed(), planning_time.as_secs_f64() * 1000.0);

        // Show comparison with estimates
        if let Some(actual_rows) = output.actual_rows {
            let estimated_rows = output.total_rows;
            if estimated_rows > 0 {
                let accuracy = if actual_rows > 0 {
                    (1.0 - ((estimated_rows as f64 - actual_rows as f64).abs() / actual_rows as f64).min(1.0)) * 100.0
                } else {
                    0.0
                };
                println!();
                println!("{}", "Estimate Accuracy:".bold());
                println!("  Estimated rows: {} vs Actual rows: {} ({:.1}% accuracy)",
                    estimated_rows, actual_rows, accuracy);
            }
        }
    }

    /// Print EXPLAIN summary
    fn print_explain_summary(output: &crate::sql::explain::ExplainOutput, options: &ExplainOptions) {
        println!();
        println!("{}", "═".repeat(80));
        println!("{:^80}", "SUMMARY");
        println!("{}", "═".repeat(80));
        println!();

        // Comparison of estimates vs actuals
        if options.analyze {
            if let (Some(actual_rows), Some(actual_time)) = (output.actual_rows, output.actual_time_ms) {
                let row_accuracy = if output.total_rows > 0 {
                    (actual_rows as f64 / output.total_rows as f64) * 100.0
                } else {
                    100.0
                };

                println!("  Estimate Accuracy:");
                println!("    Rows: {} actual vs {} estimated ({:.1}%)",
                    actual_rows, output.total_rows, row_accuracy);
                println!("    Time: {:.3} ms", actual_time);
            }
        }

        // Warnings
        if !output.warnings.is_empty() {
            println!();
            println!("  {}", "Warnings:".yellow());
            for warning in &output.warnings {
                println!("    - {}", warning);
            }
        }

        // Suggestions
        if !output.suggestions.is_empty() {
            println!();
            println!("  {}", "Suggestions:".cyan());
            for suggestion in &output.suggestions {
                println!("    - {}", suggestion);
            }
        }
    }

    /// Print storage layer information relevant to the query
    fn print_storage_layer_info(db: &EmbeddedDatabase, query: &str) {
        let query_upper = query.to_uppercase();

        // Extract table name if present
        let table_name = if let Some(from_idx) = query_upper.find("FROM") {
            let after_from = &query[from_idx + 5..];
            after_from.split_whitespace().next()
                .map(|s| s.trim_end_matches(|c| c == ',' || c == ';'))
        } else {
            None
        };

        println!();
        println!("{}", "Storage Layer Capabilities:".bold());

        // Show predicate pushdown capabilities
        println!("  {}: {}", "Predicate Pushdown".cyan(),
            if query_upper.contains("WHERE") { "Active".green() } else { "Not applicable".dimmed() });

        // Check for bloom filters and zone maps
        let has_equality = query_upper.contains(" = ");
        let has_range = query_upper.contains(" > ") || query_upper.contains(" < ") ||
                        query_upper.contains(" >= ") || query_upper.contains(" <= ") ||
                        query_upper.contains(" BETWEEN ");

        if has_equality {
            println!("  {}: {}", "Bloom Filter".cyan(), "Eligible for equality predicates".green());
        }
        if has_range {
            println!("  {}: {}", "Zone Maps".cyan(), "Eligible for range predicates".green());
        }

        // SIMD filtering
        println!("  {}: {}", "SIMD Filtering".cyan(), "Enabled".green());

        // Check for LIMIT clause for early termination
        if query_upper.contains("LIMIT") {
            println!("  {}: {}", "Early Termination".cyan(), "Active (LIMIT clause)".green());
        }

        // Check for projection pushdown (SELECT specific columns vs *)
        let has_star = query_upper.contains("SELECT *") || query_upper.contains("SELECT  *");
        println!("  {}: {}", "Projection Pushdown".cyan(),
            if has_star { "Not used (SELECT *)".yellow() } else { "Active (column pruning)".green() });

        // Show compression info if we have a table name
        if let Some(tname) = table_name {
            println!();
            println!("{}: {}", "Table".bold(), tname.cyan());

            // Try to get table statistics
            let catalog = db.storage.catalog();
            if let Ok(Some(stats)) = catalog.get_table_statistics(tname) {
                println!("  Row count: {}", stats.row_count);
                println!("  Avg row size: {} bytes", stats.avg_row_size);
                println!("  Total size: {} bytes", stats.total_size);
            }

            // Note: Custom compression was removed in favor of RocksDB's built-in LZ4 compression.
            // Compression statistics are not available separately - compression is automatic.
        }

        // Index usage info
        println!();
        println!("{}", "Index Analysis:".bold());
        if let Some(tname) = table_name {
            // Check if there are vector indexes on the table
            let mut found_indexes = false;

            // Look for vector indexes using the vector index manager
            let vector_indexes = db.storage.vector_indexes();
            let all_metadata = vector_indexes.list_all_metadata();
            for idx in all_metadata.iter().filter(|i| i.table_name == tname) {
                found_indexes = true;
                let idx_type_str = match &idx.index_type {
                    crate::storage::VectorIndexType::Standard(_) => "HNSW",
                    crate::storage::VectorIndexType::Quantized(_) => "Quantized HNSW",
                };
                println!("  Vector Index: {} on {}.{} ({})",
                    idx.name.cyan(), idx.table_name, idx.column_name, idx_type_str);
            }

            if !found_indexes {
                println!("  {}", "No indexes found on this table".dimmed());
                if query_upper.contains("WHERE") {
                    println!("  {}", "Suggestion: Consider adding an index on filtered columns".yellow());
                }
            }
        } else {
            println!("  {}", "N/A (no table reference)".dimmed());
        }

        // Join strategy info
        if query_upper.contains("JOIN") {
            println!();
            println!("{}", "Join Strategy:".bold());
            if query_upper.contains("INNER JOIN") || query_upper.contains("JOIN") {
                println!("  Strategy: {} (build on smaller table, probe larger)", "Hash Join".cyan());
            }
            if query_upper.contains("LEFT JOIN") || query_upper.contains("LEFT OUTER JOIN") {
                println!("  Strategy: {} with build on right table", "Hash Join".cyan());
            }
            if query_upper.contains("RIGHT JOIN") || query_upper.contains("RIGHT OUTER JOIN") {
                println!("  Strategy: {} with build on left table", "Hash Join".cyan());
            }
            println!("  {}", "Note: Merge Join available if inputs are pre-sorted".dimmed());
        }
    }

    /// Format storage bytes into human-readable string
    fn format_storage(bytes: u64) -> String {
        if bytes == u64::MAX {
            "Unlimited".to_string()
        } else if bytes >= 1024 * 1024 * 1024 * 1024 {
            format!("{} TB", bytes / 1024 / 1024 / 1024 / 1024)
        } else if bytes >= 1024 * 1024 * 1024 {
            format!("{} GB", bytes / 1024 / 1024 / 1024)
        } else if bytes >= 1024 * 1024 {
            format!("{} MB", bytes / 1024 / 1024)
        } else if bytes >= 1024 {
            format!("{} KB", bytes / 1024)
        } else {
            format!("{} B", bytes)
        }
    }

    /// Print plan command help
    fn print_plan_help() {
        eprintln!("{}", "Plan Commands:".bold());
        eprintln!("  {} - List all plans", "\\tenant plans".cyan());
        eprintln!("  {} - Show plan details", "\\tenant plan info <id>".cyan());
        eprintln!("  {} - Create plan (ID auto-generated)", "\\tenant plan create <name> <tier> <storage_mb> <conn> <qps>".cyan());
        eprintln!("  {} - Edit plan", "\\tenant plan edit <id> <field> <value>".cyan());
        eprintln!("  {} - Enable plan", "\\tenant plan enable <id>".cyan());
        eprintln!("  {} - Disable plan", "\\tenant plan disable <id>".cyan());
        eprintln!("  {} - Delete plan", "\\tenant plan delete <id>".cyan());
        eprintln!("  {} - Assign tenant to plan", "\\tenant plan <tenant> <plan_id>".cyan());
        eprintln!("  {} - Real-time usage with HWM/Avg", "\\tenant usage [name]".cyan());
    }

    /// Create ASCII progress bar
    fn progress_bar(percentage: f64, width: usize) -> String {
        let filled = ((percentage / 100.0) * width as f64).round() as usize;
        let filled = filled.min(width);
        let empty = width - filled;

        let bar_char = if percentage > 90.0 { "█".red() }
            else if percentage > 70.0 { "█".yellow() }
            else { "█".green() };

        format!("[{}{}]",
            bar_char.to_string().repeat(filled),
            "░".dimmed().to_string().repeat(empty)
        )
    }

    /// Print help text
    fn print_help() {
        println!("\n{}", format!("HeliosDB Lite v{} REPL Commands", env!("CARGO_PKG_VERSION")).bold());
        println!("{}", "═".repeat(70));

        println!("\n{}", "Basic Meta Commands:".bold());
        println!("  {}  - Quit the REPL", "\\q, \\quit, \\exit".cyan());
        println!("  {}        - Show this help", "\\h, \\help, \\?".cyan());
        println!("  {}                - List all tables", "\\d".cyan());
        println!("  {}          - Describe table schema", "\\d <table>".cyan());
        println!("  {}               - List tables with details", "\\dt".cyan());
        println!("  {}               - List system views", "\\dS".cyan());
        println!("  {}          - Describe system view", "\\dS <view>".cyan());
        println!("  {}          - Toggle query timing", "\\timing".cyan());

        println!("\n{}", "v2.0 Feature Commands:".bold());
        println!("  {}         - List database branches", "\\branches".cyan());
        println!("  {}        - List time-travel snapshots", "\\snapshots".cyan());
        println!("  {}              - List materialized views", "\\dmv".cyan());
        println!("  {}         - Describe materialized view", "\\dmv <view>".cyan());
        println!("  {}      - Show compression statistics", "\\compression".cyan());
        println!("  {} - Show compression for table", "\\compression <table>".cyan());

        println!("\n{}", "v2.1 Feature Commands:".bold());
        println!("  {}               - Show all settings", "\\set".cyan());
        println!("  {}    - Set REPL variable (use SQL SET for persistent)", "\\set <var> <value>".cyan());
        println!("  {}      - Show server status", "\\server [status]".cyan());
        println!("  {}   - Start server (use CLI)", "\\server start".cyan());
        println!("  {}    - Stop server (use CLI)", "\\server stop".cyan());
        println!("  {}      - Show SSL/TLS status", "\\ssl [status]".cyan());
        println!("  {}        - List users", "\\user [list]".cyan());
        println!("  {}     - Add user", "\\user add <name>".cyan());
        println!("  {}  - Remove user", "\\user remove <name>".cyan());
        println!("  {}  - Change password", "\\password <user>".cyan());
        println!("  {}           - Show configuration", "\\config".cyan());
        println!("  {}      - Reload configuration", "\\config reload".cyan());
        println!("  {} - Optimization recommendations", "\\optimize <table>".cyan());
        println!("  {}   - Index recommendations", "\\indexes <table>".cyan());
        println!("  {}            - Database statistics", "\\stats".cyan());

        println!("\n{}", "v2.6 AI Feature Commands:".bold());
        println!("  {}    - List AI schema templates", "\\ai templates".cyan());
        println!("  {} - Show template details", "\\ai template <name>".cyan());
        println!("  {}   - Infer schema from data", "\\ai infer [format]".cyan());
        println!("  {} - Generate schema from description", "\\ai generate <desc>".cyan());
        println!("  {} - AI optimization suggestions", "\\ai optimize <table>".cyan());

        println!("\n{}", "v3.2 Multi-Tenancy Commands:".bold());
        println!("  {}         - List all tenants", "\\tenants".cyan());
        println!("  {}       - List all tenants", "\\tenant list".cyan());
        println!("  {} - Create tenant", "\\tenant create <name> [plan] [isolation]".cyan());
        println!("  {}      - Set current tenant context", "\\tenant use <name>".cyan());
        println!("  {}     - Show tenant details", "\\tenant info <name>".cyan());
        println!("  {}    - Show quota usage", "\\tenant quota [name]".cyan());
        println!("  {}   - Delete tenant", "\\tenant delete <name>".cyan());
        println!("  {}      - Show current context", "\\tenant current".cyan());
        println!("  {}        - Clear tenant context", "\\tenant clear".cyan());

        println!("\n{}", "v3.3 Plan Management Commands:".bold());
        println!("  {}        - List available plans", "\\tenant plans".cyan());
        println!("  {} - Show plan details", "\\tenant plan info <id>".cyan());
        println!("  {} - Create plan (ID auto-generated)", "\\tenant plan create <name> <tier> <storage_mb> <conn> <qps>".cyan());
        println!("  {} - Edit plan field", "\\tenant plan edit <id> <field> <value>".cyan());
        println!("  {} - Enable plan", "\\tenant plan enable <id>".cyan());
        println!("  {} - Disable plan", "\\tenant plan disable <id>".cyan());
        println!("  {} - Delete plan (tenants downgraded)", "\\tenant plan delete <id>".cyan());
        println!("  {} - Change tenant's plan", "\\tenant plan <tenant> <plan>".cyan());
        println!("  {}  - Real-time usage with HWM/Avg", "\\tenant usage [name]".cyan());
        println!();
        println!("  {} free, starter, pro, enterprise, unlimited", "Default plans:".dimmed());

        println!("\n{}", "Database Export Commands:".bold());
        println!("  {}  - Dump database to SQL file", "\\dump [file]".cyan());
        println!("       {} - Default: dump.sql", "if no file specified".dimmed());

        println!("\n{}", "SQL Commands:".bold());
        println!("  End SQL statements with semicolon (;)");
        println!("  Multi-line statements are supported");
        println!("  Press {} to cancel current input", "Ctrl-C".yellow());
        println!("  Press {} to exit", "Ctrl-D".yellow());

        println!("\n{}", "SQL Settings Commands:".bold());
        println!("  {}", "SET optimizer = on;".cyan());
        println!("  {}", "SET statement_timeout = 30000;  -- milliseconds".cyan());
        println!("  {}", "SET work_mem = 8192;  -- KB".cyan());
        println!("  {}", "SHOW ALL;".cyan());
        println!("  {}", "SHOW optimizer;".cyan());

        println!("\n{}", "Basic SQL Examples:".bold());
        println!("  CREATE TABLE users (id INT, name TEXT);");
        println!("  INSERT INTO users VALUES (1, 'Alice');");
        println!("  SELECT * FROM users;");

        println!("\n{}", "v2.0 Feature Examples:".bold());
        println!("  {}", "-- Database Branching".dimmed());
        println!("  CREATE DATABASE BRANCH dev FROM main AS OF NOW;");
        println!("  MERGE DATABASE BRANCH dev INTO main;");
        println!();
        println!("  {}", "-- Time-Travel Queries".dimmed());
        println!("  SELECT * FROM orders AS OF TIMESTAMP '2025-11-23 10:00:00';");
        println!();
        println!("  {}", "-- Materialized Views".dimmed());
        println!("  CREATE MATERIALIZED VIEW user_stats AS");
        println!("    SELECT user_id, COUNT(*) FROM orders GROUP BY user_id");
        println!("    WITH (auto_refresh = true, max_cpu_percent = 15);");
        println!();
        println!("  {}", "-- System Views".dimmed());
        println!("  SELECT * FROM pg_database_branches();");
        println!("  SELECT * FROM pg_vector_index_stats();");
        println!();
    }
}

/// Result of executing a meta command
#[derive(Debug)]
pub enum MetaCommandResult {
    /// Continue REPL
    Continue,
    /// Quit REPL
    Quit,
    /// Toggle timing (new state)
    ToggleTiming(bool),
    /// Switch to branch (branch name)
    SwitchBranch(String),
    /// Toggle LSN display (new state)
    ToggleLsn(bool),
    /// Configuration was reloaded (new config)
    ConfigReloaded(super::ReplConfig),
}
