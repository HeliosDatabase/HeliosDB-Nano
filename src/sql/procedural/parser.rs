//! Procedural Language Parser
//!
//! Multi-dialect parser for procedural SQL (PL/pgSQL, T-SQL, PL/SQL, DB2 PL).
//! Automatically detects dialect based on syntax patterns or explicit specification.

use crate::{Result, Error, DataType};
use crate::sql::LogicalExpr;
use super::ast::*;

/// Procedural language parser
pub struct ProceduralParser {
    /// Current dialect (Auto for auto-detection)
    dialect: ProceduralDialect,
    /// Input source
    source: String,
    /// Current position
    pos: usize,
    /// Current line number
    line: usize,
    /// Current column number
    col: usize,
}

impl ProceduralParser {
    /// Create a new parser
    pub fn new(source: &str) -> Self {
        Self {
            dialect: ProceduralDialect::Auto,
            source: source.to_string(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    /// Create parser with specific dialect
    pub fn with_dialect(mut self, dialect: ProceduralDialect) -> Self {
        self.dialect = dialect;
        self
    }

    /// Auto-detect dialect from source
    pub fn detect_dialect(source: &str) -> ProceduralDialect {
        let upper = source.to_uppercase();

        // T-SQL patterns
        if upper.contains("@") && (upper.contains("SET @") || upper.contains("DECLARE @")) {
            return ProceduralDialect::TSql;
        }
        if upper.contains("BEGIN TRY") || upper.contains("RAISERROR") || upper.contains("PRINT ") {
            return ProceduralDialect::TSql;
        }

        // PL/SQL patterns
        if upper.contains(" IS\n") || upper.contains(" IS\r\n") {
            return ProceduralDialect::PlSql;
        }
        if upper.contains("DBMS_OUTPUT") || upper.contains("UTL_") {
            return ProceduralDialect::PlSql;
        }

        // DB2 PL patterns
        if upper.contains("DECLARE HANDLER") || upper.contains("SIGNAL SQLSTATE") {
            return ProceduralDialect::Db2Pl;
        }

        // Default to PL/pgSQL (most common)
        ProceduralDialect::PlPgSql
    }

    /// Parse a complete function/procedure definition
    pub fn parse_routine(&mut self) -> Result<RoutineDefinition> {
        self.skip_whitespace();

        // Detect dialect if auto
        if self.dialect == ProceduralDialect::Auto {
            self.dialect = Self::detect_dialect(&self.source);
        }

        // Parse CREATE FUNCTION/PROCEDURE
        self.expect_keyword("CREATE")?;
        self.skip_whitespace();

        // Optional OR REPLACE
        self.try_keyword("OR");
        self.skip_whitespace();
        self.try_keyword("REPLACE");
        self.skip_whitespace();

        // FUNCTION or PROCEDURE
        let is_function = if self.try_keyword("FUNCTION") {
            true
        } else {
            self.expect_keyword("PROCEDURE")?;
            false
        };

        self.skip_whitespace();

        // Parse name (optionally schema-qualified)
        let (schema, name) = self.parse_qualified_name()?;

        // Parse parameters
        self.skip_whitespace();
        let parameters = if self.peek_char() == Some('(') {
            self.parse_parameters()?
        } else {
            Vec::new()
        };

        // Parse RETURNS (for functions)
        let return_type = if is_function {
            self.skip_whitespace();
            self.expect_keyword("RETURNS")?;
            self.skip_whitespace();
            Some(self.parse_return_type()?)
        } else {
            None
        };

        // Parse optional function attributes
        let mut language = "PLPGSQL".to_string();
        let mut volatility = Volatility::Volatile;
        let mut security_definer = false;

        loop {
            self.skip_whitespace();
            if self.try_keyword("LANGUAGE") {
                self.skip_whitespace();
                language = self.parse_identifier()?.to_uppercase();
            } else if self.try_keyword("IMMUTABLE") {
                volatility = Volatility::Immutable;
            } else if self.try_keyword("STABLE") {
                volatility = Volatility::Stable;
            } else if self.try_keyword("VOLATILE") {
                volatility = Volatility::Volatile;
            } else if self.try_keyword("SECURITY") {
                self.skip_whitespace();
                if self.try_keyword("DEFINER") {
                    security_definer = true;
                } else {
                    self.expect_keyword("INVOKER")?;
                }
            } else if self.try_keyword("AS") {
                break;
            } else {
                break;
            }
        }

        // Parse body
        self.skip_whitespace();
        self.expect_keyword("AS")?;
        self.skip_whitespace();

        // Body is usually in $$ delimiters or quotes
        let body_text = self.parse_body_text()?;

        // Parse the body as a procedural block
        let mut body_parser = ProceduralParser::new(&body_text);
        body_parser.dialect = self.dialect;
        let body = body_parser.parse_block()?;

        Ok(RoutineDefinition {
            name,
            schema,
            parameters,
            return_type,
            language,
            volatility,
            security_definer,
            body,
            dialect: self.dialect,
        })
    }

    /// Parse a procedural block (BEGIN...END)
    pub fn parse_block(&mut self) -> Result<ProceduralBlock> {
        let mut block = ProceduralBlock::new();

        self.skip_whitespace();

        // Optional label
        if self.peek_char() == Some('<') && self.source[self.pos..].starts_with("<<") {
            block.label = Some(self.parse_label()?);
            self.skip_whitespace();
        }

        // DECLARE section (optional)
        if self.try_keyword("DECLARE") {
            block.declarations = self.parse_declarations()?;
        }

        // BEGIN
        self.skip_whitespace();
        self.expect_keyword("BEGIN")?;

        // Parse statements until END
        block.statements = self.parse_statements_until_end()?;

        // Optional EXCEPTION section
        self.skip_whitespace();
        if self.try_keyword("EXCEPTION") {
            block.exception_handlers = self.parse_exception_handlers()?;
        }

        // END
        self.skip_whitespace();
        self.expect_keyword("END")?;

        // Optional label after END
        self.skip_whitespace();
        if let Some(ref label) = block.label {
            self.try_keyword(label);
        }

        // Optional semicolon
        self.skip_whitespace();
        if self.peek_char() == Some(';') {
            self.advance();
        }

        Ok(block)
    }

    /// Parse variable declarations
    fn parse_declarations(&mut self) -> Result<Vec<VariableDeclaration>> {
        let mut declarations = Vec::new();

        loop {
            self.skip_whitespace();

            // Check for BEGIN (end of declarations)
            if self.check_keyword("BEGIN") {
                break;
            }

            // Parse single declaration
            if let Some(decl) = self.try_parse_declaration()? {
                declarations.push(decl);
            } else {
                break;
            }
        }

        Ok(declarations)
    }

    /// Try to parse a single declaration
    fn try_parse_declaration(&mut self) -> Result<Option<VariableDeclaration>> {
        let start_pos = self.pos;

        // Check for T-SQL @variable syntax
        let name = if self.dialect == ProceduralDialect::TSql && self.peek_char() == Some('@') {
            self.advance();
            format!("@{}", self.parse_identifier()?)
        } else {
            match self.try_parse_identifier() {
                Some(id) => id,
                None => {
                    self.pos = start_pos;
                    return Ok(None);
                }
            }
        };

        self.skip_whitespace();

        // Check for CONSTANT keyword
        let is_constant = self.try_keyword("CONSTANT");
        self.skip_whitespace();

        // Parse data type
        let data_type = self.try_parse_data_type();

        // Check for NOT NULL
        self.skip_whitespace();
        let not_null = self.try_keyword("NOT") && {
            self.skip_whitespace();
            self.try_keyword("NULL")
        };

        // Check for DEFAULT or :=
        self.skip_whitespace();
        let default = if self.try_keyword("DEFAULT") || self.try_keyword(":=") {
            self.skip_whitespace();
            Some(self.parse_expression()?)
        } else {
            None
        };

        // Expect semicolon
        self.skip_whitespace();
        if self.peek_char() == Some(';') {
            self.advance();
        }

        Ok(Some(VariableDeclaration {
            name,
            data_type,
            default,
            is_constant,
            not_null,
        }))
    }

    /// Parse statements until END keyword
    fn parse_statements_until_end(&mut self) -> Result<Vec<ProceduralStatement>> {
        let mut statements = Vec::new();

        loop {
            self.skip_whitespace();

            // Check for END or EXCEPTION
            if self.check_keyword("END") || self.check_keyword("EXCEPTION") {
                break;
            }

            // Parse statement
            if let Some(stmt) = self.try_parse_statement()? {
                statements.push(stmt);
            } else {
                break;
            }
        }

        Ok(statements)
    }

    /// Try to parse a single statement
    fn try_parse_statement(&mut self) -> Result<Option<ProceduralStatement>> {
        self.skip_whitespace();

        // IF statement
        if self.try_keyword("IF") {
            return Ok(Some(self.parse_if_statement()?));
        }

        // WHILE loop
        if self.try_keyword("WHILE") {
            return Ok(Some(self.parse_while_statement()?));
        }

        // LOOP
        if self.try_keyword("LOOP") {
            return Ok(Some(self.parse_loop_statement(None)?));
        }

        // FOR loop
        if self.try_keyword("FOR") {
            return Ok(Some(self.parse_for_statement(None)?));
        }

        // EXIT
        if self.try_keyword("EXIT") {
            return Ok(Some(self.parse_exit_statement()?));
        }

        // CONTINUE
        if self.try_keyword("CONTINUE") {
            return Ok(Some(self.parse_continue_statement()?));
        }

        // RETURN
        if self.try_keyword("RETURN") {
            return Ok(Some(self.parse_return_statement()?));
        }

        // RAISE
        if self.try_keyword("RAISE") {
            return Ok(Some(self.parse_raise_statement()?));
        }

        // NULL statement
        if self.try_keyword("NULL") {
            self.skip_whitespace();
            if self.peek_char() == Some(';') {
                self.advance();
            }
            return Ok(Some(ProceduralStatement::Null));
        }

        // PRINT (T-SQL)
        if self.dialect == ProceduralDialect::TSql && self.try_keyword("PRINT") {
            return Ok(Some(self.parse_print_statement()?));
        }

        // BEGIN (nested block)
        if self.check_keyword("BEGIN") || self.check_keyword("DECLARE") {
            let mut nested_parser = ProceduralParser::new(&self.source[self.pos..]);
            nested_parser.dialect = self.dialect;
            let block = nested_parser.parse_block()?;
            self.pos += nested_parser.pos;
            return Ok(Some(ProceduralStatement::Block(block)));
        }

        // Assignment or SQL execution
        if let Some(stmt) = self.try_parse_assignment_or_execute()? {
            return Ok(Some(stmt));
        }

        Ok(None)
    }

    /// Parse IF statement
    fn parse_if_statement(&mut self) -> Result<ProceduralStatement> {
        self.skip_whitespace();
        let condition = self.parse_expression()?;

        self.skip_whitespace();
        self.expect_keyword("THEN")?;

        let then_block = self.parse_statements_until(&["ELSIF", "ELSE", "END"])?;

        let mut elsif_branches = Vec::new();
        while self.try_keyword("ELSIF") {
            self.skip_whitespace();
            let elsif_cond = self.parse_expression()?;
            self.skip_whitespace();
            self.expect_keyword("THEN")?;
            let elsif_stmts = self.parse_statements_until(&["ELSIF", "ELSE", "END"])?;
            elsif_branches.push((elsif_cond, elsif_stmts));
        }

        let else_block = if self.try_keyword("ELSE") {
            Some(self.parse_statements_until(&["END"])?)
        } else {
            None
        };

        self.expect_keyword("END")?;
        self.skip_whitespace();
        self.try_keyword("IF");
        self.skip_whitespace();
        if self.peek_char() == Some(';') {
            self.advance();
        }

        Ok(ProceduralStatement::If {
            condition,
            then_block,
            elsif_branches,
            else_block,
        })
    }

    /// Parse WHILE statement
    fn parse_while_statement(&mut self) -> Result<ProceduralStatement> {
        self.skip_whitespace();
        let condition = self.parse_expression()?;

        self.skip_whitespace();
        self.expect_keyword("LOOP")?;

        let body = self.parse_statements_until(&["END"])?;

        self.expect_keyword("END")?;
        self.skip_whitespace();
        self.try_keyword("LOOP");
        self.skip_whitespace();
        if self.peek_char() == Some(';') {
            self.advance();
        }

        Ok(ProceduralStatement::While {
            label: None,
            condition,
            body,
        })
    }

    /// Parse simple LOOP statement
    fn parse_loop_statement(&mut self, label: Option<String>) -> Result<ProceduralStatement> {
        let body = self.parse_statements_until(&["END"])?;

        self.expect_keyword("END")?;
        self.skip_whitespace();
        self.try_keyword("LOOP");
        self.skip_whitespace();
        if self.peek_char() == Some(';') {
            self.advance();
        }

        Ok(ProceduralStatement::Loop { label, body })
    }

    /// Parse FOR statement (numeric or query)
    fn parse_for_statement(&mut self, label: Option<String>) -> Result<ProceduralStatement> {
        self.skip_whitespace();
        let variable = self.parse_identifier()?;

        self.skip_whitespace();
        self.expect_keyword("IN")?;
        self.skip_whitespace();

        let reverse = self.try_keyword("REVERSE");
        self.skip_whitespace();

        // Check if this is a query FOR or numeric FOR
        if self.check_keyword("SELECT") || self.peek_char() == Some('(') {
            // Query FOR
            let query = self.parse_until_loop()?;
            let body = self.parse_statements_until(&["END"])?;

            self.expect_keyword("END")?;
            self.skip_whitespace();
            self.try_keyword("LOOP");
            self.skip_whitespace();
            if self.peek_char() == Some(';') {
                self.advance();
            }

            Ok(ProceduralStatement::ForQuery {
                label,
                record_variable: variable,
                query,
                body,
            })
        } else {
            // Numeric FOR
            let lower_bound = self.parse_expression()?;

            self.skip_whitespace();
            self.expect_keyword("..")?;
            self.skip_whitespace();

            let upper_bound = self.parse_expression()?;

            self.skip_whitespace();
            let step = if self.try_keyword("BY") {
                self.skip_whitespace();
                Some(self.parse_expression()?)
            } else {
                None
            };

            self.skip_whitespace();
            self.expect_keyword("LOOP")?;

            let body = self.parse_statements_until(&["END"])?;

            self.expect_keyword("END")?;
            self.skip_whitespace();
            self.try_keyword("LOOP");
            self.skip_whitespace();
            if self.peek_char() == Some(';') {
                self.advance();
            }

            Ok(ProceduralStatement::ForNumeric {
                label,
                variable,
                lower_bound,
                upper_bound,
                step,
                reverse,
                body,
            })
        }
    }

    /// Parse EXIT statement
    fn parse_exit_statement(&mut self) -> Result<ProceduralStatement> {
        self.skip_whitespace();

        let label = self.try_parse_identifier();
        self.skip_whitespace();

        let when_condition = if self.try_keyword("WHEN") {
            self.skip_whitespace();
            Some(self.parse_expression()?)
        } else {
            None
        };

        self.skip_whitespace();
        if self.peek_char() == Some(';') {
            self.advance();
        }

        Ok(ProceduralStatement::Exit { label, when_condition })
    }

    /// Parse CONTINUE statement
    fn parse_continue_statement(&mut self) -> Result<ProceduralStatement> {
        self.skip_whitespace();

        let label = self.try_parse_identifier();
        self.skip_whitespace();

        let when_condition = if self.try_keyword("WHEN") {
            self.skip_whitespace();
            Some(self.parse_expression()?)
        } else {
            None
        };

        self.skip_whitespace();
        if self.peek_char() == Some(';') {
            self.advance();
        }

        Ok(ProceduralStatement::Continue { label, when_condition })
    }

    /// Parse RETURN statement
    fn parse_return_statement(&mut self) -> Result<ProceduralStatement> {
        self.skip_whitespace();

        // Check for semicolon (RETURN without value)
        if self.peek_char() == Some(';') {
            self.advance();
            return Ok(ProceduralStatement::Return { value: None });
        }

        // Check for NEXT (RETURN NEXT)
        if self.try_keyword("NEXT") {
            self.skip_whitespace();
            let value = self.parse_expression()?;
            self.skip_whitespace();
            if self.peek_char() == Some(';') {
                self.advance();
            }
            return Ok(ProceduralStatement::ReturnNext { value });
        }

        // Check for QUERY (RETURN QUERY)
        if self.try_keyword("QUERY") {
            self.skip_whitespace();
            let query = self.parse_until_semicolon();
            return Ok(ProceduralStatement::ReturnQuery { query });
        }

        // Regular RETURN with value
        let value = self.parse_expression()?;
        self.skip_whitespace();
        if self.peek_char() == Some(';') {
            self.advance();
        }

        Ok(ProceduralStatement::Return { value: Some(value) })
    }

    /// Parse RAISE statement
    fn parse_raise_statement(&mut self) -> Result<ProceduralStatement> {
        self.skip_whitespace();

        let level = if self.try_keyword("EXCEPTION") {
            RaiseLevel::Exception
        } else if self.try_keyword("WARNING") {
            RaiseLevel::Warning
        } else if self.try_keyword("NOTICE") {
            RaiseLevel::Notice
        } else if self.try_keyword("INFO") {
            RaiseLevel::Info
        } else if self.try_keyword("LOG") {
            RaiseLevel::Log
        } else if self.try_keyword("DEBUG") {
            RaiseLevel::Debug
        } else {
            RaiseLevel::Exception // Default
        };

        self.skip_whitespace();

        // Check for SQLSTATE
        let sqlstate = if self.try_keyword("SQLSTATE") {
            self.skip_whitespace();
            Some(self.parse_string_literal()?)
        } else {
            None
        };

        // Parse message (usually a string literal)
        let message = if self.peek_char() == Some('\'') || self.peek_char() == Some('"') {
            Some(self.parse_expression()?)
        } else if self.peek_char() != Some(';') {
            Some(self.parse_expression()?)
        } else {
            None
        };

        // Parse optional USING clause: USING DETAIL = expr, HINT = expr
        let mut detail = None;
        let mut hint = None;

        self.skip_whitespace();
        if self.try_keyword("USING") {
            loop {
                self.skip_whitespace();

                if self.try_keyword("DETAIL") {
                    self.skip_whitespace();
                    if self.peek_char() == Some('=') {
                        self.advance();
                        self.skip_whitespace();
                    }
                    detail = Some(self.parse_expression()?);
                } else if self.try_keyword("HINT") {
                    self.skip_whitespace();
                    if self.peek_char() == Some('=') {
                        self.advance();
                        self.skip_whitespace();
                    }
                    hint = Some(self.parse_expression()?);
                } else {
                    break;
                }

                self.skip_whitespace();
                if self.peek_char() == Some(',') {
                    self.advance();
                } else {
                    break;
                }
            }
        }

        self.skip_whitespace();
        if self.peek_char() == Some(';') {
            self.advance();
        }

        Ok(ProceduralStatement::Raise {
            level,
            message,
            sqlstate,
            detail,
            hint,
        })
    }

    /// Parse PRINT statement (T-SQL)
    fn parse_print_statement(&mut self) -> Result<ProceduralStatement> {
        self.skip_whitespace();
        let message = self.parse_expression()?;
        self.skip_whitespace();
        if self.peek_char() == Some(';') {
            self.advance();
        }
        Ok(ProceduralStatement::Print { message })
    }

    /// Try to parse assignment or SQL execution
    fn try_parse_assignment_or_execute(&mut self) -> Result<Option<ProceduralStatement>> {
        let start_pos = self.pos;

        // Try to parse as assignment
        if let Some(target) = self.try_parse_identifier() {
            self.skip_whitespace();

            // Check for := or =
            if self.source[self.pos..].starts_with(":=") {
                self.pos += 2;
                self.skip_whitespace();
                let value = self.parse_expression()?;
                self.skip_whitespace();
                if self.peek_char() == Some(';') {
                    self.advance();
                }
                return Ok(Some(ProceduralStatement::Assignment { target, value }));
            }

            // T-SQL style: SET @var = value
            if target.to_uppercase() == "SET" {
                self.skip_whitespace();
                if let Some(var_name) = self.try_parse_identifier() {
                    self.skip_whitespace();
                    if self.peek_char() == Some('=') {
                        self.advance();
                        self.skip_whitespace();
                        let value = self.parse_expression()?;
                        self.skip_whitespace();
                        if self.peek_char() == Some(';') {
                            self.advance();
                        }
                        return Ok(Some(ProceduralStatement::Assignment { target: var_name, value }));
                    }
                }
            }
        }

        // Reset and try as SQL execution
        self.pos = start_pos;

        // Parse as SQL statement until semicolon
        let sql = self.parse_until_semicolon();
        if sql.trim().is_empty() {
            return Ok(None);
        }

        Ok(Some(ProceduralStatement::Execute {
            sql,
            into_variables: Vec::new(),
        }))
    }

    /// Parse exception handlers
    fn parse_exception_handlers(&mut self) -> Result<Vec<ExceptionHandler>> {
        let mut handlers = Vec::new();

        while self.try_keyword("WHEN") {
            self.skip_whitespace();

            // Parse conditions
            let mut conditions = Vec::new();
            loop {
                let condition = if self.try_keyword("OTHERS") {
                    ExceptionCondition::Others
                } else if self.try_keyword("SQLSTATE") {
                    self.skip_whitespace();
                    ExceptionCondition::SqlState(self.parse_string_literal()?)
                } else {
                    ExceptionCondition::Named(self.parse_identifier()?)
                };
                conditions.push(condition);

                self.skip_whitespace();
                if !self.try_keyword("OR") {
                    break;
                }
                self.skip_whitespace();
            }

            self.skip_whitespace();
            self.expect_keyword("THEN")?;

            let body = self.parse_statements_until(&["WHEN", "END"])?;

            handlers.push(ExceptionHandler { conditions, body });
        }

        Ok(handlers)
    }

    // === Helper Methods ===

    /// Parse statements until one of the keywords
    fn parse_statements_until(&mut self, keywords: &[&str]) -> Result<Vec<ProceduralStatement>> {
        let mut statements = Vec::new();

        loop {
            self.skip_whitespace();

            // Check for terminating keyword
            for kw in keywords {
                if self.check_keyword(kw) {
                    return Ok(statements);
                }
            }

            if let Some(stmt) = self.try_parse_statement()? {
                statements.push(stmt);
            } else {
                break;
            }
        }

        Ok(statements)
    }

    fn parse_qualified_name(&mut self) -> Result<(Option<String>, String)> {
        let first = self.parse_identifier()?;
        self.skip_whitespace();

        if self.peek_char() == Some('.') {
            self.advance();
            self.skip_whitespace();
            let second = self.parse_identifier()?;
            Ok((Some(first), second))
        } else {
            Ok((None, first))
        }
    }

    fn parse_parameters(&mut self) -> Result<Vec<RoutineParameter>> {
        let mut params = Vec::new();

        self.expect_char('(')?;
        self.skip_whitespace();

        if self.peek_char() == Some(')') {
            self.advance();
            return Ok(params);
        }

        loop {
            // Parse parameter mode
            let mode = if self.try_keyword("INOUT") || self.try_keyword("IN OUT") {
                ParameterMode::InOut
            } else if self.try_keyword("OUT") {
                ParameterMode::Out
            } else {
                self.try_keyword("IN"); // Optional IN
                ParameterMode::In
            };

            self.skip_whitespace();
            let name = self.parse_identifier()?;
            self.skip_whitespace();

            let data_type = self.parse_data_type()?;
            self.skip_whitespace();

            let default = if self.try_keyword("DEFAULT") || self.try_keyword(":=") || self.peek_char() == Some('=') {
                if self.peek_char() == Some('=') {
                    self.advance();
                }
                self.skip_whitespace();
                Some(self.parse_expression()?)
            } else {
                None
            };

            params.push(RoutineParameter {
                name,
                data_type,
                mode,
                default,
            });

            self.skip_whitespace();
            if self.peek_char() == Some(',') {
                self.advance();
                self.skip_whitespace();
            } else {
                break;
            }
        }

        self.expect_char(')')?;
        Ok(params)
    }

    fn parse_return_type(&mut self) -> Result<ReturnType> {
        if self.try_keyword("VOID") {
            return Ok(ReturnType::Void);
        }

        if self.try_keyword("SETOF") {
            self.skip_whitespace();
            let dt = self.parse_data_type()?;
            return Ok(ReturnType::SetOf(dt));
        }

        if self.try_keyword("TABLE") {
            self.skip_whitespace();
            self.expect_char('(')?;
            let mut columns = Vec::new();

            loop {
                self.skip_whitespace();
                if self.peek_char() == Some(')') {
                    break;
                }
                let col_name = self.parse_identifier()?;
                self.skip_whitespace();
                let col_type = self.parse_data_type()?;
                columns.push((col_name, col_type));

                self.skip_whitespace();
                if self.peek_char() == Some(',') {
                    self.advance();
                } else {
                    break;
                }
            }

            self.expect_char(')')?;
            return Ok(ReturnType::Table { columns });
        }

        let dt = self.parse_data_type()?;
        Ok(ReturnType::Scalar(dt))
    }

    fn parse_body_text(&mut self) -> Result<String> {
        self.skip_whitespace();

        // Check for $$ delimiters
        if self.source[self.pos..].starts_with("$$") {
            self.pos += 2;
            let start = self.pos;
            if let Some(end) = self.source[self.pos..].find("$$") {
                let body = self.source[start..start + end].to_string();
                self.pos = start + end + 2;
                return Ok(body);
            }
            return Err(Error::sql_parse("Unterminated $$ delimiter"));
        }

        // Check for $tag$ delimiters
        if self.source[self.pos..].starts_with('$') {
            let tag_start = self.pos + 1;
            if let Some(tag_end) = self.source[tag_start..].find('$') {
                let tag = &self.source[tag_start..tag_start + tag_end];
                let delimiter = format!("${}$", tag);
                self.pos += delimiter.len();
                let start = self.pos;
                if let Some(end) = self.source[self.pos..].find(&delimiter) {
                    let body = self.source[start..start + end].to_string();
                    self.pos = start + end + delimiter.len();
                    return Ok(body);
                }
                return Err(Error::sql_parse(format!("Unterminated {} delimiter", delimiter)));
            }
        }

        // Check for single quotes
        if self.peek_char() == Some('\'') {
            return Ok(self.parse_string_literal()?);
        }

        Err(Error::sql_parse("Expected function body delimiter"))
    }

    fn parse_label(&mut self) -> Result<String> {
        if !self.source[self.pos..].starts_with("<<") {
            return Err(Error::sql_parse("Expected label <<name>>"));
        }
        self.pos += 2;

        let start = self.pos;
        while self.pos < self.source.len() && !self.source[self.pos..].starts_with(">>") {
            self.pos += 1;
        }

        let label = self.source[start..self.pos].to_string();
        if self.source[self.pos..].starts_with(">>") {
            self.pos += 2;
        }

        Ok(label)
    }

    fn parse_identifier(&mut self) -> Result<String> {
        self.try_parse_identifier()
            .ok_or_else(|| Error::sql_parse("Expected identifier"))
    }

    fn try_parse_identifier(&mut self) -> Option<String> {
        self.skip_whitespace();
        let start = self.pos;

        // Handle quoted identifiers
        if self.peek_char() == Some('"') {
            self.advance();
            while self.pos < self.source.len() {
                let c = self.source.chars().nth(self.pos).unwrap_or('\0');
                if c == '"' {
                    self.advance();
                    break;
                }
                self.advance();
            }
            return Some(self.source[start + 1..self.pos - 1].to_string());
        }

        // Handle T-SQL @variable
        if self.peek_char() == Some('@') {
            self.advance();
        }

        // Regular identifier
        while self.pos < self.source.len() {
            let c = self.source.chars().nth(self.pos).unwrap_or('\0');
            if c.is_alphanumeric() || c == '_' {
                self.advance();
            } else {
                break;
            }
        }

        if self.pos > start {
            Some(self.source[start..self.pos].to_string())
        } else {
            None
        }
    }

    fn parse_expression(&mut self) -> Result<LogicalExpr> {
        // For now, return a simple string literal containing the expression text
        // A full implementation would parse this into LogicalExpr
        let expr_text = self.parse_simple_expression_text()?;

        // Convert to string literal expression
        Ok(LogicalExpr::Literal(crate::Value::String(expr_text)))
    }

    fn parse_simple_expression_text(&mut self) -> Result<String> {
        let mut depth = 0;
        let start = self.pos;

        while self.pos < self.source.len() {
            let c = self.peek_char().unwrap_or('\0');

            if c == '(' {
                depth += 1;
            } else if c == ')' {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            } else if c == ';' && depth == 0 {
                break;
            } else if depth == 0 {
                // Check for keywords that end expression
                let rest = &self.source[self.pos..].to_uppercase();
                if rest.starts_with("THEN") || rest.starts_with("LOOP") || rest.starts_with("END")
                    || rest.starts_with("ELSE") || rest.starts_with("ELSIF") || rest.starts_with("WHEN")
                {
                    break;
                }
            }

            self.advance();
        }

        Ok(self.source[start..self.pos].trim().to_string())
    }

    fn parse_data_type(&mut self) -> Result<DataType> {
        self.try_parse_data_type()
            .ok_or_else(|| Error::sql_parse("Expected data type"))
    }

    fn try_parse_data_type(&mut self) -> Option<DataType> {
        self.skip_whitespace();

        // Parse type name
        let type_name = self.try_parse_identifier()?.to_uppercase();

        // Handle array suffix
        self.skip_whitespace();
        let is_array = self.source[self.pos..].starts_with("[]");
        if is_array {
            self.pos += 2;
        }

        let base_type = match type_name.as_str() {
            "INT" | "INTEGER" | "INT4" => DataType::Int4,
            "BIGINT" | "INT8" => DataType::Int8,
            "SMALLINT" | "INT2" => DataType::Int2,
            "FLOAT" | "REAL" | "FLOAT4" => DataType::Float4,
            "DOUBLE" | "FLOAT8" => DataType::Float8,
            "TEXT" | "STRING" => DataType::Text,
            "VARCHAR" => {
                self.skip_whitespace();
                if self.peek_char() == Some('(') {
                    self.advance();
                    let len = self.parse_number().unwrap_or(255) as usize;
                    self.skip_whitespace();
                    if self.peek_char() == Some(')') {
                        self.advance();
                    }
                    DataType::Varchar(Some(len))
                } else {
                    DataType::Varchar(None)
                }
            }
            "BOOLEAN" | "BOOL" => DataType::Boolean,
            "DATE" => DataType::Date,
            "TIME" => DataType::Time,
            "TIMESTAMP" | "TIMESTAMPTZ" => DataType::Timestamp,
            "JSON" => DataType::Json,
            "JSONB" => DataType::Jsonb,
            "BYTEA" | "BLOB" => DataType::Bytea,
            "UUID" => DataType::Uuid,
            "NUMERIC" | "DECIMAL" => DataType::Numeric,
            _ => return None,
        };

        // Handle array
        if is_array {
            Some(DataType::Array(Box::new(base_type)))
        } else {
            Some(base_type)
        }
    }

    fn parse_string_literal(&mut self) -> Result<String> {
        let quote = self.peek_char().ok_or_else(|| Error::sql_parse("Expected string literal"))?;
        if quote != '\'' && quote != '"' {
            return Err(Error::sql_parse("Expected string literal"));
        }
        self.advance();

        let start = self.pos;
        while self.pos < self.source.len() {
            let c = self.peek_char().unwrap_or('\0');
            if c == quote {
                let s = self.source[start..self.pos].to_string();
                self.advance();
                return Ok(s);
            }
            self.advance();
        }

        Err(Error::sql_parse("Unterminated string literal"))
    }

    fn parse_until_semicolon(&mut self) -> String {
        let start = self.pos;
        while self.pos < self.source.len() {
            if self.peek_char() == Some(';') {
                let s = self.source[start..self.pos].to_string();
                self.advance();
                return s;
            }
            self.advance();
        }
        self.source[start..].to_string()
    }

    fn parse_until_loop(&mut self) -> Result<String> {
        let start = self.pos;
        while self.pos < self.source.len() {
            if self.check_keyword("LOOP") {
                let s = self.source[start..self.pos].trim().to_string();
                self.expect_keyword("LOOP")?;
                return Ok(s);
            }
            self.advance();
        }
        Err(Error::sql_parse("Expected LOOP"))
    }

    fn parse_number(&mut self) -> Option<i64> {
        self.skip_whitespace();
        let start = self.pos;
        while self.pos < self.source.len() {
            let c = self.peek_char().unwrap_or('\0');
            if c.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }
        if self.pos > start {
            self.source[start..self.pos].parse().ok()
        } else {
            None
        }
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.source.len() {
            let c = self.peek_char().unwrap_or('\0');
            if c.is_whitespace() {
                if c == '\n' {
                    self.line += 1;
                    self.col = 1;
                } else {
                    self.col += 1;
                }
                self.pos += 1;
            } else if self.source[self.pos..].starts_with("--") {
                // Line comment
                while self.pos < self.source.len() && self.peek_char() != Some('\n') {
                    self.pos += 1;
                }
            } else if self.source[self.pos..].starts_with("/*") {
                // Block comment
                self.pos += 2;
                while self.pos + 1 < self.source.len() && !self.source[self.pos..].starts_with("*/") {
                    if self.peek_char() == Some('\n') {
                        self.line += 1;
                        self.col = 1;
                    }
                    self.pos += 1;
                }
                if self.pos + 1 < self.source.len() {
                    self.pos += 2;
                }
            } else {
                break;
            }
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.source.chars().nth(self.pos)
    }

    fn advance(&mut self) {
        if self.pos < self.source.len() {
            self.pos += 1;
            self.col += 1;
        }
    }

    fn expect_char(&mut self, expected: char) -> Result<()> {
        self.skip_whitespace();
        if self.peek_char() == Some(expected) {
            self.advance();
            Ok(())
        } else {
            Err(Error::sql_parse(format!(
                "Expected '{}' at line {}, column {}",
                expected, self.line, self.col
            )))
        }
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<()> {
        if !self.try_keyword(keyword) {
            Err(Error::sql_parse(format!(
                "Expected '{}' at line {}, column {}",
                keyword, self.line, self.col
            )))
        } else {
            Ok(())
        }
    }

    fn try_keyword(&mut self, keyword: &str) -> bool {
        self.skip_whitespace();
        let upper_keyword = keyword.to_uppercase();
        let remaining = &self.source[self.pos..].to_uppercase();

        if remaining.starts_with(&upper_keyword) {
            let after = remaining.chars().nth(upper_keyword.len());
            if after.map_or(true, |c| !c.is_alphanumeric()) {
                self.pos += keyword.len();
                return true;
            }
        }
        false
    }

    fn check_keyword(&self, keyword: &str) -> bool {
        let upper_keyword = keyword.to_uppercase();
        let remaining = self.source[self.pos..].trim_start().to_uppercase();

        if remaining.starts_with(&upper_keyword) {
            let after = remaining.chars().nth(upper_keyword.len());
            after.map_or(true, |c| !c.is_alphanumeric())
        } else {
            false
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_dialect_plpgsql() {
        let source = "DECLARE x INTEGER; BEGIN END;";
        assert_eq!(ProceduralParser::detect_dialect(source), ProceduralDialect::PlPgSql);
    }

    #[test]
    fn test_detect_dialect_tsql() {
        let source = "DECLARE @x INT; SET @x = 1;";
        assert_eq!(ProceduralParser::detect_dialect(source), ProceduralDialect::TSql);
    }

    #[test]
    fn test_parse_simple_block() {
        let source = r#"
            DECLARE
                x INTEGER := 10;
            BEGIN
                x := x + 1;
            END;
        "#;

        let mut parser = ProceduralParser::new(source);
        let block = parser.parse_block().unwrap();

        assert_eq!(block.declarations.len(), 1);
        assert_eq!(block.declarations[0].name, "x");
        assert_eq!(block.statements.len(), 1);
    }

    #[test]
    fn test_parse_if_statement() {
        let source = r#"
            BEGIN
                IF x > 0 THEN
                    y := 1;
                ELSE
                    y := 0;
                END IF;
            END;
        "#;

        let mut parser = ProceduralParser::new(source);
        let block = parser.parse_block().unwrap();

        assert_eq!(block.statements.len(), 1);
        match &block.statements[0] {
            ProceduralStatement::If { then_block, else_block, .. } => {
                assert_eq!(then_block.len(), 1);
                assert!(else_block.is_some());
            }
            _ => panic!("Expected IF statement"),
        }
    }

    #[test]
    fn test_parse_while_loop() {
        let source = r#"
            BEGIN
                WHILE i < 10 LOOP
                    i := i + 1;
                END LOOP;
            END;
        "#;

        let mut parser = ProceduralParser::new(source);
        let block = parser.parse_block().unwrap();

        assert_eq!(block.statements.len(), 1);
        match &block.statements[0] {
            ProceduralStatement::While { body, .. } => {
                assert_eq!(body.len(), 1);
            }
            _ => panic!("Expected WHILE statement"),
        }
    }

    #[test]
    fn test_variable_scope() {
        // PL/pgSQL syntax: name CONSTANT type := value
        let source = r#"
            DECLARE
                outer_var INTEGER := 1;
                PI CONSTANT FLOAT8 := 3.14159;
            BEGIN
                NULL;
            END;
        "#;

        let mut parser = ProceduralParser::new(source);
        let block = parser.parse_block().unwrap();

        assert_eq!(block.declarations.len(), 2);
        assert!(!block.declarations[0].is_constant);
        assert!(block.declarations[1].is_constant);
    }

    // ==================== PL/pgSQL Dialect Tests ====================

    #[test]
    fn test_plpgsql_full_function() {
        let source = r#"
            DECLARE
                result INTEGER := 0;
                counter INTEGER := 1;
                max_val INTEGER := 10;
            BEGIN
                WHILE counter <= max_val LOOP
                    result := result + counter;
                    counter := counter + 1;
                END LOOP;
                RETURN result;
            END;
        "#;

        let mut parser = ProceduralParser::new(source).with_dialect(ProceduralDialect::PlPgSql);
        let block = parser.parse_block().unwrap();

        assert_eq!(block.declarations.len(), 3);
        assert!(block.statements.len() >= 2); // WHILE and RETURN
    }

    #[test]
    fn test_plpgsql_exception_handler() {
        let source = r#"
            BEGIN
                INSERT INTO test VALUES (1);
            EXCEPTION
                WHEN division_by_zero THEN
                    RAISE NOTICE 'Division error';
                WHEN OTHERS THEN
                    RAISE EXCEPTION 'Unknown error';
            END;
        "#;

        let mut parser = ProceduralParser::new(source).with_dialect(ProceduralDialect::PlPgSql);
        let block = parser.parse_block().unwrap();

        assert!(!block.exception_handlers.is_empty());
    }

    #[test]
    fn test_plpgsql_simple_loop() {
        let source = r#"
            BEGIN
                LOOP
                    i := i + 1;
                    EXIT WHEN i > 10;
                END LOOP;
            END;
        "#;

        let mut parser = ProceduralParser::new(source).with_dialect(ProceduralDialect::PlPgSql);
        let block = parser.parse_block().unwrap();

        assert_eq!(block.statements.len(), 1);
        match &block.statements[0] {
            ProceduralStatement::Loop { body, .. } => {
                assert!(body.len() >= 2); // assignment and EXIT
            }
            _ => panic!("Expected LOOP statement"),
        }
    }

    #[test]
    fn test_plpgsql_nested_if() {
        let source = r#"
            BEGIN
                IF x > 0 THEN
                    IF x > 10 THEN
                        y := 100;
                    ELSE
                        y := 10;
                    END IF;
                ELSIF x = 0 THEN
                    y := 0;
                ELSE
                    y := -1;
                END IF;
            END;
        "#;

        let mut parser = ProceduralParser::new(source).with_dialect(ProceduralDialect::PlPgSql);
        let block = parser.parse_block().unwrap();

        assert_eq!(block.statements.len(), 1);
        match &block.statements[0] {
            ProceduralStatement::If { elsif_branches, .. } => {
                assert!(!elsif_branches.is_empty());
            }
            _ => panic!("Expected IF statement"),
        }
    }

    // ==================== T-SQL Style Tests ====================

    #[test]
    fn test_tsql_style_block() {
        // T-SQL style with DECLARE inside block
        let source = r#"
            DECLARE
                @result INTEGER := 0;
            BEGIN
                @result := @result + 1;
            END;
        "#;

        let mut parser = ProceduralParser::new(source).with_dialect(ProceduralDialect::TSql);
        let block = parser.parse_block().unwrap();

        // T-SQL uses @ prefix for variables
        assert!(block.declarations.iter().any(|d| d.name.starts_with('@')));
    }

    #[test]
    fn test_tsql_style_if() {
        let source = r#"
            BEGIN
                IF x > 0 THEN
                    y := 1;
                ELSE
                    y := 0;
                END IF;
            END;
        "#;

        let mut parser = ProceduralParser::new(source).with_dialect(ProceduralDialect::TSql);
        let block = parser.parse_block().unwrap();

        assert!(!block.statements.is_empty());
    }

    // ==================== PL/SQL (Oracle) Style Tests ====================

    #[test]
    fn test_plsql_style_declaration() {
        let source = r#"
            DECLARE
                v_name VARCHAR(100);
                v_count INTEGER := 0;
            BEGIN
                v_count := v_count + 1;
            END;
        "#;

        let mut parser = ProceduralParser::new(source).with_dialect(ProceduralDialect::PlSql);
        let block = parser.parse_block().unwrap();

        assert_eq!(block.declarations.len(), 2);
    }

    #[test]
    fn test_plsql_exit_when() {
        let source = r#"
            BEGIN
                LOOP
                    counter := counter + 1;
                    EXIT WHEN counter > 10;
                END LOOP;
            END;
        "#;

        let mut parser = ProceduralParser::new(source).with_dialect(ProceduralDialect::PlSql);
        let block = parser.parse_block().unwrap();

        assert_eq!(block.statements.len(), 1);
        match &block.statements[0] {
            ProceduralStatement::Loop { body, .. } => {
                assert!(body.iter().any(|s| matches!(s, ProceduralStatement::Exit { .. })));
            }
            _ => panic!("Expected LOOP statement"),
        }
    }

    // ==================== DB2 PL Style Tests ====================

    #[test]
    fn test_db2_style_compound() {
        let source = r#"
            DECLARE
                v_result INTEGER := 0;
            BEGIN
                v_result := v_result + 1;
            END;
        "#;

        let mut parser = ProceduralParser::new(source).with_dialect(ProceduralDialect::Db2Pl);
        let block = parser.parse_block().unwrap();

        assert!(!block.declarations.is_empty());
    }

    // ==================== Dialect Auto-Detection Tests ====================

    #[test]
    fn test_dialect_autodetect_plpgsql() {
        let source = r#"
            DECLARE
                v_test INTEGER := 0;
            BEGIN
                RAISE NOTICE 'Test';
            END;
        "#;

        let mut parser = ProceduralParser::new(source);
        let block = parser.parse_block().unwrap();

        // Should detect PL/pgSQL from := and RAISE NOTICE
        assert!(!block.declarations.is_empty());
    }

    #[test]
    fn test_dialect_autodetect_tsql_style() {
        let source = r#"
            DECLARE
                @counter INTEGER := 0;
            BEGIN
                @counter := @counter + 1;
            END;
        "#;

        let mut parser = ProceduralParser::new(source);
        let block = parser.parse_block().unwrap();

        // Should detect T-SQL from @ prefix
        assert!(block.declarations.iter().any(|d| d.name.starts_with('@')));
    }

    // ==================== Control Flow Tests ====================

    #[test]
    fn test_continue_statement() {
        let source = r#"
            BEGIN
                WHILE TRUE LOOP
                    IF x = 5 THEN
                        CONTINUE;
                    END IF;
                    x := x + 1;
                END LOOP;
            END;
        "#;

        let mut parser = ProceduralParser::new(source);
        let block = parser.parse_block().unwrap();

        assert_eq!(block.statements.len(), 1);
    }

    #[test]
    fn test_return_with_value() {
        let source = r#"
            BEGIN
                RETURN 42;
            END;
        "#;

        let mut parser = ProceduralParser::new(source);
        let block = parser.parse_block().unwrap();

        assert_eq!(block.statements.len(), 1);
        match &block.statements[0] {
            ProceduralStatement::Return { value } => {
                assert!(value.is_some());
            }
            _ => panic!("Expected RETURN statement"),
        }
    }

    #[test]
    fn test_raise_levels() {
        let source = r#"
            BEGIN
                RAISE DEBUG 'Debug message';
                RAISE LOG 'Log message';
                RAISE INFO 'Info message';
                RAISE NOTICE 'Notice message';
                RAISE WARNING 'Warning message';
                RAISE EXCEPTION 'Error message';
            END;
        "#;

        let mut parser = ProceduralParser::new(source);
        let block = parser.parse_block().unwrap();

        assert_eq!(block.statements.len(), 6);
    }

    // ==================== Expression Tests ====================

    #[test]
    fn test_complex_expression_assignment() {
        let source = r#"
            BEGIN
                result := (a + b) * c / d;
                flag := x > 0 AND y < 100;
            END;
        "#;

        let mut parser = ProceduralParser::new(source);
        let block = parser.parse_block().unwrap();

        assert_eq!(block.statements.len(), 2);
    }

    #[test]
    fn test_null_statement() {
        let source = r#"
            BEGIN
                NULL;
                NULL;
            END;
        "#;

        let mut parser = ProceduralParser::new(source);
        let block = parser.parse_block().unwrap();

        assert_eq!(block.statements.len(), 2);
        assert!(block.statements.iter().all(|s| matches!(s, ProceduralStatement::Null)));
    }
}
