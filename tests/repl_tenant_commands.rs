//! REPL Multi-Tenancy Command Tests for HeliosDB-Lite
//!
//! Tests cover:
//! - \tenants - List all tenants
//! - \tenant create - Create new tenant with optional plan
//! - \tenant use - Set current tenant context
//! - \tenant info - Show tenant details
//! - \tenant quota - Show quota usage
//! - \tenant plan - Change tenant plan
//! - \tenant delete - Delete tenant
//! - \tenant current - Show current context
//! - \tenant clear - Clear tenant context
//!
//! NOTE: These tests are disabled because they use internal APIs that are not
//! publicly exposed. They need to be rewritten to use the public API.
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_nano::repl::commands::MetaCommand;

// ============================================================================
// Command Parsing Tests
// ============================================================================

#[test]
fn test_parse_tenant_list() {
    let cmd = MetaCommand::parse("\\tenants");
    assert!(cmd.is_some());
    assert_eq!(cmd.unwrap(), MetaCommand::TenantList);

    // Alternative form
    let cmd = MetaCommand::parse("\\tenant list");
    assert!(cmd.is_some());
    assert_eq!(cmd.unwrap(), MetaCommand::TenantList);
}

#[test]
fn test_parse_tenant_create_with_name_only() {
    let cmd = MetaCommand::parse("\\tenant create mycompany");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantCreate { name, plan } => {
            assert_eq!(name, "mycompany");
            assert_eq!(plan, None);
        }
        _ => panic!("Expected TenantCreate"),
    }
}

#[test]
fn test_parse_tenant_create_with_plan() {
    let cmd = MetaCommand::parse("\\tenant create mycompany pro");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantCreate { name, plan } => {
            assert_eq!(name, "mycompany");
            assert_eq!(plan, Some("pro".to_string()));
        }
        _ => panic!("Expected TenantCreate"),
    }
}

#[test]
fn test_parse_tenant_create_various_plans() {
    let plans = vec!["free", "starter", "pro", "enterprise"];

    for plan_name in plans {
        let cmd = MetaCommand::parse(&format!("\\tenant create test {}", plan_name));
        assert!(cmd.is_some());

        match cmd.unwrap() {
            MetaCommand::TenantCreate { name, plan } => {
                assert_eq!(name, "test");
                assert_eq!(plan, Some(plan_name.to_string()));
            }
            _ => panic!("Expected TenantCreate"),
        }
    }
}

#[test]
fn test_parse_tenant_use() {
    let cmd = MetaCommand::parse("\\tenant use mycompany");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantUse(tenant_ref) => {
            assert_eq!(tenant_ref, "mycompany");
        }
        _ => panic!("Expected TenantUse"),
    }
}

#[test]
fn test_parse_tenant_use_with_uuid() {
    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    let cmd = MetaCommand::parse(&format!("\\tenant use {}", uuid));
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantUse(tenant_ref) => {
            assert_eq!(tenant_ref, uuid);
        }
        _ => panic!("Expected TenantUse"),
    }
}

#[test]
fn test_parse_tenant_info() {
    let cmd = MetaCommand::parse("\\tenant info mycompany");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantInfo(tenant_ref) => {
            assert_eq!(tenant_ref, "mycompany");
        }
        _ => panic!("Expected TenantInfo"),
    }
}

#[test]
fn test_parse_tenant_quota_with_name() {
    let cmd = MetaCommand::parse("\\tenant quota mycompany");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantQuota(tenant_ref) => {
            assert!(tenant_ref.is_some());
            assert_eq!(tenant_ref.unwrap(), "mycompany");
        }
        _ => panic!("Expected TenantQuota"),
    }
}

#[test]
fn test_parse_tenant_quota_current() {
    let cmd = MetaCommand::parse("\\tenant quota");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantQuota(tenant_ref) => {
            assert!(tenant_ref.is_none(), "Should use current tenant when no name provided");
        }
        _ => panic!("Expected TenantQuota"),
    }
}

#[test]
fn test_parse_tenant_plan() {
    let cmd = MetaCommand::parse("\\tenant plan mycompany enterprise");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantPlan { tenant, plan } => {
            assert_eq!(tenant, "mycompany");
            assert_eq!(plan, "enterprise");
        }
        _ => panic!("Expected TenantPlan"),
    }
}

#[test]
fn test_parse_tenant_delete() {
    let cmd = MetaCommand::parse("\\tenant delete mycompany");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantDelete(tenant_ref) => {
            assert_eq!(tenant_ref, "mycompany");
        }
        _ => panic!("Expected TenantDelete"),
    }
}

#[test]
fn test_parse_tenant_current() {
    let cmd = MetaCommand::parse("\\tenant current");
    assert!(cmd.is_some());
    assert_eq!(cmd.unwrap(), MetaCommand::TenantCurrent);
}

#[test]
fn test_parse_tenant_clear() {
    let cmd = MetaCommand::parse("\\tenant clear");
    assert!(cmd.is_some());
    assert_eq!(cmd.unwrap(), MetaCommand::TenantClearContext);
}

#[test]
fn test_parse_tenant_shorthand_info() {
    // Shorthand: \tenant <name> should work as \tenant info <name>
    let cmd = MetaCommand::parse("\\tenant mycompany");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantInfo(tenant_ref) => {
            assert_eq!(tenant_ref, "mycompany");
        }
        _ => panic!("Expected TenantInfo (shorthand)"),
    }
}

// ============================================================================
// Invalid Command Tests
// ============================================================================

#[test]
fn test_parse_tenant_create_missing_name() {
    let cmd = MetaCommand::parse("\\tenant create");
    // Should return None or handle gracefully
    // Based on implementation, this might print error and return None
    // We can't test the error message here, but we verify it doesn't crash
    println!("Testing missing tenant name: {:?}", cmd);
}

#[test]
fn test_parse_tenant_use_missing_name() {
    let cmd = MetaCommand::parse("\\tenant use");
    // Should return None or handle gracefully
    println!("Testing missing tenant reference: {:?}", cmd);
}

#[test]
fn test_parse_tenant_plan_missing_plan() {
    let cmd = MetaCommand::parse("\\tenant plan mycompany");
    // Should return None or handle gracefully
    println!("Testing missing plan name: {:?}", cmd);
}

// ============================================================================
// Command Format Tests
// ============================================================================

#[test]
fn test_tenant_command_with_whitespace() {
    // Extra whitespace should be handled
    let cmd = MetaCommand::parse("  \\tenant   create   mycompany   pro  ");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantCreate { name, plan } => {
            assert_eq!(name, "mycompany");
            assert_eq!(plan, Some("pro".to_string()));
        }
        _ => panic!("Expected TenantCreate"),
    }
}

#[test]
fn test_tenant_command_case_sensitivity() {
    // Command itself should be case-sensitive (lowercase)
    let cmd1 = MetaCommand::parse("\\tenant create MyCompany");
    let cmd2 = MetaCommand::parse("\\TENANT CREATE MyCompany");

    // \tenant should work, \TENANT might not
    assert!(cmd1.is_some());
    // Capital TENANT is not recognized
    assert!(cmd2.is_none());
}

#[test]
fn test_tenant_name_with_special_characters() {
    // Tenant names with hyphens, underscores
    let cmd = MetaCommand::parse("\\tenant create my-company_123");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantCreate { name, plan } => {
            assert_eq!(name, "my-company_123");
        }
        _ => panic!("Expected TenantCreate"),
    }
}

// ============================================================================
// Plan Validation Tests (Logical)
// ============================================================================

#[test]
fn test_all_supported_plans() {
    let supported_plans = vec!["free", "starter", "pro", "enterprise"];

    for plan in supported_plans {
        let cmd = MetaCommand::parse(&format!("\\tenant create test {}", plan));
        assert!(cmd.is_some(), "Plan '{}' should be parseable", plan);
    }
}

#[test]
fn test_custom_plan_names() {
    // The parser should accept any plan name, validation happens at execution
    let custom_plans = vec!["custom", "premium", "unlimited", "trial"];

    for plan in custom_plans {
        let cmd = MetaCommand::parse(&format!("\\tenant create test {}", plan));
        assert!(cmd.is_some());

        match cmd.unwrap() {
            MetaCommand::TenantCreate { name, plan: parsed_plan } => {
                assert_eq!(parsed_plan, Some(plan.to_string()));
            }
            _ => panic!("Expected TenantCreate"),
        }
    }
}

// ============================================================================
// Integration-Style Tests (Execution Behavior)
// ============================================================================

#[test]
fn test_tenant_workflow_commands_parseable() {
    // Simulate a complete workflow
    let workflow = vec![
        "\\tenants",
        "\\tenant create acme_corp pro",
        "\\tenant info acme_corp",
        "\\tenant use acme_corp",
        "\\tenant current",
        "\\tenant quota",
        "\\tenant plan acme_corp enterprise",
        "\\tenant quota acme_corp",
        "\\tenant clear",
        "\\tenant delete acme_corp",
    ];

    for (i, command) in workflow.iter().enumerate() {
        let cmd = MetaCommand::parse(command);
        assert!(cmd.is_some(),
                "Command {} failed to parse: {}", i + 1, command);
    }
}

#[test]
fn test_tenant_list_aliases() {
    // Test all ways to list tenants
    let aliases = vec![
        "\\tenants",
        "\\tenant list",
    ];

    for alias in aliases {
        let cmd = MetaCommand::parse(alias);
        assert!(cmd.is_some());
        assert_eq!(cmd.unwrap(), MetaCommand::TenantList,
                   "Alias '{}' should map to TenantList", alias);
    }
}

// ============================================================================
// Edge Cases
// ============================================================================

#[test]
fn test_empty_tenant_command() {
    let cmd = MetaCommand::parse("\\tenant");
    // Should return None or handle gracefully
    // Implementation might show help or error
    println!("Empty tenant command: {:?}", cmd);
}

#[test]
fn test_tenant_command_without_backslash() {
    let cmd = MetaCommand::parse("tenant create mycompany");
    assert!(cmd.is_none(), "Commands without \\ should not parse");
}

#[test]
fn test_tenant_unknown_subcommand() {
    let cmd = MetaCommand::parse("\\tenant unknown_command");
    // Should either return None or be treated as shorthand for info
    // Based on implementation, unknown subcommands might be treated as tenant names
    println!("Unknown subcommand: {:?}", cmd);
}

#[test]
fn test_tenant_with_uuid_partial() {
    // UUIDs can be matched with partial prefix
    let cmd = MetaCommand::parse("\\tenant use 550e8400");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantUse(tenant_ref) => {
            assert_eq!(tenant_ref, "550e8400");
        }
        _ => panic!("Expected TenantUse"),
    }
}

// ============================================================================
// Documentation Tests
// ============================================================================

#[test]
fn test_tenant_command_examples_from_help() {
    // Examples that should appear in help text
    let examples = vec![
        ("\\tenants", "List all tenants"),
        ("\\tenant create acme", "Create tenant"),
        ("\\tenant create acme pro", "Create with plan"),
        ("\\tenant use acme", "Switch tenant"),
        ("\\tenant info acme", "Show details"),
        ("\\tenant quota", "Show current quota"),
        ("\\tenant quota acme", "Show specific quota"),
        ("\\tenant plan acme enterprise", "Change plan"),
        ("\\tenant current", "Show current context"),
        ("\\tenant clear", "Clear context"),
        ("\\tenant delete acme", "Delete tenant"),
    ];

    for (command, description) in examples {
        let cmd = MetaCommand::parse(command);
        assert!(cmd.is_some(),
                "Example failed: {} ({})", command, description);
    }
}

// ============================================================================
// Command Equality Tests
// ============================================================================

#[test]
fn test_tenant_command_equality() {
    let cmd1 = MetaCommand::parse("\\tenant create test pro");
    let cmd2 = MetaCommand::parse("\\tenant create test pro");

    assert_eq!(cmd1, cmd2, "Identical commands should be equal");
}

#[test]
fn test_tenant_list_equality() {
    let cmd1 = MetaCommand::parse("\\tenants");
    let cmd2 = MetaCommand::parse("\\tenant list");

    assert_eq!(cmd1, cmd2, "Different aliases should produce equal commands");
}

// ============================================================================
// Multi-Word Tenant Names (if supported)
// ============================================================================

#[test]
fn test_single_word_tenant_name() {
    let cmd = MetaCommand::parse("\\tenant create mycompany");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantCreate { name, .. } => {
            assert_eq!(name, "mycompany");
        }
        _ => panic!("Expected TenantCreate"),
    }
}

#[test]
fn test_tenant_name_with_numbers() {
    let cmd = MetaCommand::parse("\\tenant create company123");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantCreate { name, .. } => {
            assert_eq!(name, "company123");
        }
        _ => panic!("Expected TenantCreate"),
    }
}

// ============================================================================
// Plan Tier Tests
// ============================================================================

#[test]
fn test_plan_free() {
    let cmd = MetaCommand::parse("\\tenant create test free");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantCreate { plan, .. } => {
            assert_eq!(plan, Some("free".to_string()));
        }
        _ => panic!("Expected TenantCreate"),
    }
}

#[test]
fn test_plan_starter() {
    let cmd = MetaCommand::parse("\\tenant create test starter");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantCreate { plan, .. } => {
            assert_eq!(plan, Some("starter".to_string()));
        }
        _ => panic!("Expected TenantCreate"),
    }
}

#[test]
fn test_plan_pro() {
    let cmd = MetaCommand::parse("\\tenant create test pro");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantCreate { plan, .. } => {
            assert_eq!(plan, Some("pro".to_string()));
        }
        _ => panic!("Expected TenantCreate"),
    }
}

#[test]
fn test_plan_enterprise() {
    let cmd = MetaCommand::parse("\\tenant create test enterprise");
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantCreate { plan, .. } => {
            assert_eq!(plan, Some("enterprise".to_string()));
        }
        _ => panic!("Expected TenantCreate"),
    }
}

// ============================================================================
// Command Chaining Tests (Logical Order)
// ============================================================================

#[test]
fn test_create_then_use_workflow() {
    let commands = vec![
        MetaCommand::parse("\\tenant create newcompany"),
        MetaCommand::parse("\\tenant use newcompany"),
        MetaCommand::parse("\\tenant current"),
    ];

    assert!(commands.iter().all(|c| c.is_some()),
            "Create-use-current workflow should parse");
}

#[test]
fn test_info_then_plan_workflow() {
    let commands = vec![
        MetaCommand::parse("\\tenant info acme"),
        MetaCommand::parse("\\tenant plan acme enterprise"),
        MetaCommand::parse("\\tenant quota acme"),
    ];

    assert!(commands.iter().all(|c| c.is_some()),
            "Info-plan-quota workflow should parse");
}

// ============================================================================
// Stress Tests
// ============================================================================

#[test]
fn test_many_tenant_commands() {
    // Ensure parser can handle many commands
    for i in 0..100 {
        let cmd = MetaCommand::parse(&format!("\\tenant create tenant{}", i));
        assert!(cmd.is_some());
    }
}

#[test]
fn test_long_tenant_name() {
    let long_name = "a".repeat(100);
    let cmd = MetaCommand::parse(&format!("\\tenant create {}", long_name));
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantCreate { name, .. } => {
            assert_eq!(name, long_name);
        }
        _ => panic!("Expected TenantCreate"),
    }
}

#[test]
fn test_unicode_tenant_name() {
    let unicode_name = "公司テスト企業";
    let cmd = MetaCommand::parse(&format!("\\tenant create {}", unicode_name));
    assert!(cmd.is_some());

    match cmd.unwrap() {
        MetaCommand::TenantCreate { name, .. } => {
            assert_eq!(name, unicode_name);
        }
        _ => panic!("Expected TenantCreate"),
    }
}
