//! PostgREST-Compatible API
//!
//! Implements the PostgREST API specification used by Supabase
//! for automatic REST API generation from database tables.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// PostgREST query parameters
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PostgRestQuery {
    /// Select columns (e.g., "id,name,email")
    pub select: Option<String>,
    /// Order by (e.g., "created_at.desc")
    pub order: Option<String>,
    /// Limit results
    pub limit: Option<usize>,
    /// Offset results
    pub offset: Option<usize>,
    /// Return count (exact, planned, estimated)
    pub count: Option<CountType>,
}

/// Count type for responses
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CountType {
    Exact,
    Planned,
    Estimated,
}

/// PostgREST filter operators
#[derive(Debug, Clone)]
pub enum FilterOperator {
    /// Equal (eq.)
    Eq,
    /// Not equal (neq.)
    Neq,
    /// Greater than (gt.)
    Gt,
    /// Greater than or equal (gte.)
    Gte,
    /// Less than (lt.)
    Lt,
    /// Less than or equal (lte.)
    Lte,
    /// Pattern match (like.)
    Like,
    /// Case-insensitive pattern (ilike.)
    ILike,
    /// Is null (is.)
    Is,
    /// In list (in.)
    In,
    /// Contains (cs.)
    Contains,
    /// Contained by (cd.)
    ContainedBy,
    /// Overlaps (ov.)
    Overlaps,
    /// Full-text search (fts.)
    Fts,
    /// Plain full-text (plfts.)
    PlainFts,
    /// Phrase full-text (phfts.)
    PhraseFts,
    /// Websearch full-text (wfts.)
    WebFts,
    /// Not (not.)
    Not,
    /// Or (or.)
    Or,
    /// And (and.)
    And,
}

impl FilterOperator {
    /// Parse operator from string prefix
    pub fn from_str(s: &str) -> Option<(Self, &str)> {
        let ops = [
            ("eq.", Self::Eq),
            ("neq.", Self::Neq),
            ("gt.", Self::Gt),
            ("gte.", Self::Gte),
            ("lt.", Self::Lt),
            ("lte.", Self::Lte),
            ("like.", Self::Like),
            ("ilike.", Self::ILike),
            ("is.", Self::Is),
            ("in.", Self::In),
            ("cs.", Self::Contains),
            ("cd.", Self::ContainedBy),
            ("ov.", Self::Overlaps),
            ("fts.", Self::Fts),
            ("plfts.", Self::PlainFts),
            ("phfts.", Self::PhraseFts),
            ("wfts.", Self::WebFts),
            ("not.", Self::Not),
            ("or.", Self::Or),
            ("and.", Self::And),
        ];

        for (prefix, op) in ops {
            if s.starts_with(prefix) {
                return Some((op, &s[prefix.len()..]));
            }
        }

        None
    }

    /// Convert to SQL operator
    pub fn to_sql(&self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Neq => "!=",
            Self::Gt => ">",
            Self::Gte => ">=",
            Self::Lt => "<",
            Self::Lte => "<=",
            Self::Like => "LIKE",
            Self::ILike => "ILIKE",
            Self::Is => "IS",
            Self::In => "IN",
            Self::Contains => "@>",
            Self::ContainedBy => "<@",
            Self::Overlaps => "&&",
            Self::Fts | Self::PlainFts | Self::PhraseFts | Self::WebFts => "@@",
            Self::Not => "NOT",
            Self::Or => "OR",
            Self::And => "AND",
        }
    }
}

/// Parsed filter condition
#[derive(Debug, Clone)]
pub struct Filter {
    pub column: String,
    pub operator: FilterOperator,
    pub value: String,
    pub negated: bool,
}

/// PostgREST request builder
pub struct PostgRestBuilder {
    table: String,
    select: Option<String>,
    filters: Vec<Filter>,
    order: Vec<(String, bool)>, // (column, desc)
    limit: Option<usize>,
    offset: Option<usize>,
    upsert: bool,
    on_conflict: Option<String>,
    returning: Option<String>,
}

impl PostgRestBuilder {
    pub fn new(table: &str) -> Self {
        Self {
            table: table.to_string(),
            select: None,
            filters: Vec::new(),
            order: Vec::new(),
            limit: None,
            offset: None,
            upsert: false,
            on_conflict: None,
            returning: None,
        }
    }

    /// Set columns to select
    pub fn select(mut self, columns: &str) -> Self {
        self.select = Some(columns.to_string());
        self
    }

    /// Add filter
    pub fn filter(mut self, column: &str, op: FilterOperator, value: &str) -> Self {
        self.filters.push(Filter {
            column: column.to_string(),
            operator: op,
            value: value.to_string(),
            negated: false,
        });
        self
    }

    /// Add order by
    pub fn order(mut self, column: &str, desc: bool) -> Self {
        self.order.push((column.to_string(), desc));
        self
    }

    /// Set limit
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Set offset
    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Enable upsert mode
    pub fn upsert(mut self, on_conflict: Option<&str>) -> Self {
        self.upsert = true;
        self.on_conflict = on_conflict.map(|s| s.to_string());
        self
    }

    /// Set returning columns
    pub fn returning(mut self, columns: &str) -> Self {
        self.returning = Some(columns.to_string());
        self
    }

    /// Build SELECT query
    pub fn build_select(&self) -> String {
        let columns = self.select.as_deref().unwrap_or("*");
        let mut sql = format!("SELECT {} FROM {}", columns, self.table);

        if !self.filters.is_empty() {
            let conditions: Vec<String> = self.filters.iter()
                .map(|f| {
                    let op = f.operator.to_sql();
                    let prefix = if f.negated { "NOT " } else { "" };

                    match f.operator {
                        FilterOperator::In => {
                            let values = f.value.trim_matches(|c| c == '(' || c == ')')
                                .split(',')
                                .map(|v| format!("'{}'", v.trim()))
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("{}{} {} ({})", prefix, f.column, op, values)
                        }
                        FilterOperator::Is => {
                            format!("{}{} {} {}", prefix, f.column, op, f.value.to_uppercase())
                        }
                        FilterOperator::Like | FilterOperator::ILike => {
                            format!("{}{} {} '{}'", prefix, f.column, op, f.value.replace('*', "%"))
                        }
                        _ => {
                            format!("{}{} {} '{}'", prefix, f.column, op, f.value)
                        }
                    }
                })
                .collect();

            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }

        if !self.order.is_empty() {
            let orders: Vec<String> = self.order.iter()
                .map(|(col, desc)| {
                    if *desc {
                        format!("{} DESC", col)
                    } else {
                        format!("{} ASC", col)
                    }
                })
                .collect();

            sql.push_str(" ORDER BY ");
            sql.push_str(&orders.join(", "));
        }

        if let Some(limit) = self.limit {
            sql.push_str(&format!(" LIMIT {}", limit));
        }

        if let Some(offset) = self.offset {
            sql.push_str(&format!(" OFFSET {}", offset));
        }

        sql
    }

    /// Build INSERT query
    pub fn build_insert(&self, data: &[HashMap<String, serde_json::Value>]) -> String {
        if data.is_empty() {
            return String::new();
        }

        let columns: Vec<&String> = data[0].keys().collect();
        let col_names = columns.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", ");

        let values: Vec<String> = data.iter()
            .map(|row| {
                let vals: Vec<String> = columns.iter()
                    .map(|col| {
                        match row.get(*col) {
                            Some(serde_json::Value::String(s)) => format!("'{}'", s.replace('\'', "''")),
                            Some(serde_json::Value::Null) => "NULL".to_string(),
                            Some(v) => v.to_string(),
                            None => "NULL".to_string(),
                        }
                    })
                    .collect();
                format!("({})", vals.join(", "))
            })
            .collect();

        let mut sql = if self.upsert {
            let on_conflict = self.on_conflict.as_deref().unwrap_or("id");
            format!(
                "INSERT INTO {} ({}) VALUES {} ON CONFLICT ({}) DO UPDATE SET {}",
                self.table,
                col_names,
                values.join(", "),
                on_conflict,
                columns.iter()
                    .filter(|c| *c != &on_conflict)
                    .map(|c| format!("{} = EXCLUDED.{}", c, c))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            format!(
                "INSERT INTO {} ({}) VALUES {}",
                self.table,
                col_names,
                values.join(", ")
            )
        };

        if let Some(ref returning) = self.returning {
            sql.push_str(&format!(" RETURNING {}", returning));
        }

        sql
    }

    /// Build UPDATE query
    pub fn build_update(&self, data: &HashMap<String, serde_json::Value>) -> String {
        let sets: Vec<String> = data.iter()
            .map(|(col, val)| {
                match val {
                    serde_json::Value::String(s) => format!("{} = '{}'", col, s.replace('\'', "''")),
                    serde_json::Value::Null => format!("{} = NULL", col),
                    _ => format!("{} = {}", col, val),
                }
            })
            .collect();

        let mut sql = format!("UPDATE {} SET {}", self.table, sets.join(", "));

        if !self.filters.is_empty() {
            let conditions: Vec<String> = self.filters.iter()
                .map(|f| format!("{} {} '{}'", f.column, f.operator.to_sql(), f.value))
                .collect();
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }

        if let Some(ref returning) = self.returning {
            sql.push_str(&format!(" RETURNING {}", returning));
        }

        sql
    }

    /// Build DELETE query
    pub fn build_delete(&self) -> String {
        let mut sql = format!("DELETE FROM {}", self.table);

        if !self.filters.is_empty() {
            let conditions: Vec<String> = self.filters.iter()
                .map(|f| format!("{} {} '{}'", f.column, f.operator.to_sql(), f.value))
                .collect();
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }

        if let Some(ref returning) = self.returning {
            sql.push_str(&format!(" RETURNING {}", returning));
        }

        sql
    }
}

/// Parse PostgREST query parameters from URL query string
pub fn parse_query_params(params: &HashMap<String, String>) -> (PostgRestQuery, Vec<Filter>) {
    let mut query = PostgRestQuery::default();
    let mut filters = Vec::new();

    for (key, value) in params {
        match key.as_str() {
            "select" => query.select = Some(value.clone()),
            "order" => query.order = Some(value.clone()),
            "limit" => query.limit = value.parse().ok(),
            "offset" => query.offset = value.parse().ok(),
            "count" => {
                query.count = match value.as_str() {
                    "exact" => Some(CountType::Exact),
                    "planned" => Some(CountType::Planned),
                    "estimated" => Some(CountType::Estimated),
                    _ => None,
                };
            }
            _ => {
                // Try to parse as filter
                if let Some((op, val)) = FilterOperator::from_str(value) {
                    filters.push(Filter {
                        column: key.clone(),
                        operator: op,
                        value: val.to_string(),
                        negated: false,
                    });
                }
            }
        }
    }

    (query, filters)
}

/// Parse order parameter (e.g., "created_at.desc,name.asc")
pub fn parse_order(order: &str) -> Vec<(String, bool)> {
    order.split(',')
        .filter_map(|part| {
            let parts: Vec<&str> = part.trim().split('.').collect();
            if parts.is_empty() {
                return None;
            }

            let column = parts[0].to_string();
            let desc = parts.get(1).map(|d| *d == "desc").unwrap_or(false);

            Some((column, desc))
        })
        .collect()
}

/// Parse select parameter with embedded resources
/// e.g., "id,name,posts(id,title,comments(id,body))"
pub fn parse_select(select: &str) -> SelectTree {
    let mut tree = SelectTree::new();
    parse_select_recursive(select, &mut tree);
    tree
}

/// Select tree for nested resource selection
#[derive(Debug, Clone, Default)]
pub struct SelectTree {
    pub columns: Vec<String>,
    pub embedded: HashMap<String, SelectTree>,
}

impl SelectTree {
    pub fn new() -> Self {
        Self::default()
    }
}

fn parse_select_recursive(select: &str, tree: &mut SelectTree) {
    let mut current = String::new();
    let mut depth = 0;

    for ch in select.chars() {
        match ch {
            '(' => {
                depth += 1;
                if depth == 1 {
                    // Start of embedded resource
                    continue;
                }
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    // End of embedded resource - parse inner select
                    if !current.is_empty() {
                        if let Some(last_col) = tree.columns.last().cloned() {
                            let mut embedded_tree = SelectTree::new();
                            parse_select_recursive(&current, &mut embedded_tree);
                            tree.embedded.insert(last_col, embedded_tree);
                        }
                        current.clear();
                    }
                    continue;
                }
            }
            ',' if depth == 0 => {
                if !current.is_empty() {
                    tree.columns.push(current.trim().to_string());
                    current.clear();
                }
                continue;
            }
            _ => {}
        }

        current.push(ch);
    }

    if !current.is_empty() {
        tree.columns.push(current.trim().to_string());
    }
}

/// Response headers for PostgREST
#[derive(Debug, Clone, Default)]
pub struct PostgRestHeaders {
    /// Content-Range header (for pagination)
    pub content_range: Option<String>,
    /// Preference-Applied header
    pub preference_applied: Option<String>,
    /// Location header (for created resources)
    pub location: Option<String>,
}

impl PostgRestHeaders {
    pub fn with_range(mut self, offset: usize, limit: usize, total: Option<usize>) -> Self {
        let end = offset + limit - 1;
        self.content_range = Some(match total {
            Some(t) => format!("{}-{}/{}", offset, end.min(t - 1), t),
            None => format!("{}-{}/*", offset, end),
        });
        self
    }

    pub fn with_location(mut self, location: &str) -> Self {
        self.location = Some(location.to_string());
        self
    }

    pub fn to_hashmap(&self) -> HashMap<String, String> {
        let mut headers = HashMap::new();

        if let Some(ref range) = self.content_range {
            headers.insert("Content-Range".to_string(), range.clone());
        }

        if let Some(ref pref) = self.preference_applied {
            headers.insert("Preference-Applied".to_string(), pref.clone());
        }

        if let Some(ref loc) = self.location {
            headers.insert("Location".to_string(), loc.clone());
        }

        headers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_operator_parsing() {
        assert!(matches!(
            FilterOperator::from_str("eq.hello"),
            Some((FilterOperator::Eq, "hello"))
        ));

        assert!(matches!(
            FilterOperator::from_str("gte.100"),
            Some((FilterOperator::Gte, "100"))
        ));
    }

    #[test]
    fn test_build_select() {
        let builder = PostgRestBuilder::new("users")
            .select("id,name,email")
            .filter("status", FilterOperator::Eq, "active")
            .order("created_at", true)
            .limit(10);

        let sql = builder.build_select();
        assert!(sql.contains("SELECT id,name,email FROM users"));
        assert!(sql.contains("WHERE status = 'active'"));
        assert!(sql.contains("ORDER BY created_at DESC"));
        assert!(sql.contains("LIMIT 10"));
    }

    #[test]
    fn test_parse_order() {
        let orders = parse_order("created_at.desc,name.asc");
        assert_eq!(orders.len(), 2);
        assert_eq!(orders[0], ("created_at".to_string(), true));
        assert_eq!(orders[1], ("name".to_string(), false));
    }

    #[test]
    fn test_parse_select_embedded() {
        let tree = parse_select("id,name,posts(id,title)");
        assert!(tree.columns.contains(&"id".to_string()));
        assert!(tree.columns.contains(&"name".to_string()));
        assert!(tree.columns.contains(&"posts".to_string()));
        assert!(tree.embedded.contains_key("posts"));
    }
}
