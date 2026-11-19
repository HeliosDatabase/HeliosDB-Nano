//! Procedural Language Parser
//!
//! Multi-dialect parser for procedural SQL (PL/pgSQL, T-SQL, PL/SQL, DB2 PL).
//! Automatically detects dialect based on syntax patterns or explicit specification.

use super::ast::*;
use crate::sql::LogicalExpr;
use crate::{DataType, Error, Result};

/// What ends an embedded SQL fragment (an expression or a query). A `;` at
/// nesting depth 0, and a `)` that closes a parenthesis opened before the
/// fragment, always end it.
#[derive(Clone, Copy, Default)]
struct Stops {
    /// `,` at nesting depth 0 (USING options, parameter lists).
    comma: bool,
    /// `..` at nesting depth 0 (numeric FOR bounds).
    dotdot: bool,
    /// Whole keywords that end the fragment at nesting depth 0, outside any
    /// `CASE … END` of the fragment itself.
    keywords: &'static [&'static str],
}

impl Stops {
    /// Stop at these whole keywords (and at `;`).
    const fn keywords(keywords: &'static [&'static str]) -> Self {
        Self {
            comma: false,
            dotdot: false,
            keywords,
        }
    }
}

/// A character that may continue an identifier (`$` is legal after the first
/// character in PostgreSQL).
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// A character that may start an identifier or keyword.
fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

/// `s` starts with keyword `kw` (ASCII, any case). A keyword ending in a word
/// character must also END there: `END` does not match `endpoint`.
fn starts_with_keyword(s: &str, kw: &str) -> bool {
    let (sb, kb) = (s.as_bytes(), kw.as_bytes());
    match sb.get(..kb.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(kb) => {}
        _ => return false,
    }
    let word_like = kw.chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_');
    if !word_like {
        return true;
    }
    // The matched prefix is ASCII, so `kb.len()` is a character boundary.
    s.get(kb.len()..)
        .and_then(|r| r.chars().next())
        .is_none_or(|c| !is_ident_char(c))
}

/// Length of the dollar-quote delimiter (`$$` or `$tag$`) starting `s`.
/// `$1` and `$name` (the routine-variable sigil) are not one.
fn dollar_tag_len(s: &str) -> Option<usize> {
    let mut chars = s.char_indices();
    if !matches!(chars.next(), Some((_, '$'))) {
        return None;
    }
    for (i, c) in chars {
        if c == '$' {
            return Some(i + 1);
        }
        if !(c.is_alphanumeric() || c == '_') || (i == 1 && c.is_ascii_digit()) {
            return None;
        }
    }
    None
}

/// If a string literal, quoted identifier, dollar-quoted string or comment
/// starts `s`, its length in bytes (the rest of `s` when unterminated).
/// `escape_string` marks an `E'…'` literal, where a backslash escapes.
fn quoted_len(s: &str, escape_string: bool) -> Option<usize> {
    let bytes = s.as_bytes();
    match bytes.first()? {
        q @ (b'\'' | b'"') => {
            let q = *q;
            let mut i = 1;
            while let Some(&b) = bytes.get(i) {
                if b == b'\\' && escape_string && q == b'\'' {
                    i += 2;
                } else if b == q {
                    if bytes.get(i + 1) == Some(&q) {
                        i += 2;
                    } else {
                        return Some(i + 1);
                    }
                } else {
                    i += 1;
                }
            }
            Some(bytes.len())
        }
        b'$' => {
            let t = dollar_tag_len(s)?;
            let tag = s.get(..t)?;
            Some(s.get(t..)?.find(tag).map_or(s.len(), |e| t + e + t))
        }
        b'-' if s.starts_with("--") => Some(s.find('\n').unwrap_or(s.len())),
        b'/' if s.starts_with("/*") => Some(s.find("*/").map_or(s.len(), |e| e + 2)),
        _ => None,
    }
}

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
        if self.rest().starts_with("<<") {
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
            let label = label.clone();
            self.try_keyword(&label);
        }

        self.eat_semicolon();

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

        // Check for DEFAULT, := or =
        self.skip_whitespace();
        let default = if self.try_keyword("DEFAULT") || self.try_keyword(":=") || self.try_keyword("=") {
            self.skip_whitespace();
            Some(self.parse_expression(Stops::default())?)
        } else {
            None
        };

        self.eat_semicolon();

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
        self.parse_statements_until(&["END", "EXCEPTION"])
    }

    /// Try to parse a single statement
    fn try_parse_statement(&mut self) -> Result<Option<ProceduralStatement>> {
        self.skip_whitespace();

        // `<<label>>` before a loop labels the loop; before DECLARE/BEGIN it
        // labels a nested block (the nested parser reads it).
        if self.rest().starts_with("<<") {
            let save = (self.pos, self.line, self.col);
            let label = self.parse_label()?;
            self.skip_whitespace();
            if self.try_keyword("LOOP") {
                return Ok(Some(self.parse_loop_statement(Some(label))?));
            }
            if self.try_keyword("WHILE") {
                return Ok(Some(self.parse_while_statement(Some(label))?));
            }
            if self.try_keyword("FOR") {
                return Ok(Some(self.parse_for_statement(Some(label))?));
            }
            (self.pos, self.line, self.col) = save;
            return Ok(Some(self.parse_nested_block()?));
        }

        // IF statement
        if self.try_keyword("IF") {
            return Ok(Some(self.parse_if_statement()?));
        }

        // WHILE loop
        if self.try_keyword("WHILE") {
            return Ok(Some(self.parse_while_statement(None)?));
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
            self.eat_semicolon();
            return Ok(Some(ProceduralStatement::Null));
        }

        // PERFORM query: run it as a SELECT and discard the result.
        if self.try_keyword("PERFORM") {
            self.skip_whitespace();
            let query = self.scan_fragment(Stops::default());
            self.eat_semicolon();
            if query.is_empty() {
                return Err(Error::sql_parse(format!(
                    "PERFORM needs a query at line {}, column {}",
                    self.line, self.col
                )));
            }
            return Ok(Some(ProceduralStatement::Execute {
                sql: format!("SELECT {}", query),
                into_variables: Vec::new(),
            }));
        }

        // CASE statement (searched or simple). A CASE *expression* never
        // starts a statement, so this cannot steal SQL.
        if self.try_keyword("CASE") {
            return Ok(Some(self.parse_case_statement()?));
        }

        // PRINT (T-SQL)
        if self.dialect == ProceduralDialect::TSql && self.try_keyword("PRINT") {
            return Ok(Some(self.parse_print_statement()?));
        }

        // BEGIN (nested block)
        if self.check_keyword("BEGIN") || self.check_keyword("DECLARE") {
            return Ok(Some(self.parse_nested_block()?));
        }

        // Assignment or SQL execution
        self.try_parse_assignment_or_execute()
    }

    /// A nested `[<<label>>] [DECLARE …] BEGIN … END;` block.
    fn parse_nested_block(&mut self) -> Result<ProceduralStatement> {
        let mut nested_parser = ProceduralParser::new(self.rest());
        nested_parser.dialect = self.dialect;
        let block = nested_parser.parse_block()?;
        let consumed = nested_parser.pos;
        self.advance_bytes(consumed);
        Ok(ProceduralStatement::Block(block))
    }

    /// `END <kind> [label];` closing a loop, IF or CASE.
    fn finish_end(&mut self, kind: &str, label: Option<&str>) -> Result<()> {
        self.expect_keyword("END")?;
        self.skip_whitespace();
        self.try_keyword(kind);
        if let Some(label) = label {
            self.skip_whitespace();
            self.try_keyword(label);
        }
        self.eat_semicolon();
        Ok(())
    }

    /// Parse IF statement
    fn parse_if_statement(&mut self) -> Result<ProceduralStatement> {
        self.skip_whitespace();
        let condition = self.parse_expression(Stops::keywords(&["THEN"]))?;

        self.skip_whitespace();
        self.expect_keyword("THEN")?;

        let then_block = self.parse_statements_until(&["ELSIF", "ELSEIF", "ELSE", "END"])?;

        let mut elsif_branches = Vec::new();
        while self.try_keyword("ELSIF") || self.try_keyword("ELSEIF") {
            self.skip_whitespace();
            let elsif_cond = self.parse_expression(Stops::keywords(&["THEN"]))?;
            self.skip_whitespace();
            self.expect_keyword("THEN")?;
            let elsif_stmts = self.parse_statements_until(&["ELSIF", "ELSEIF", "ELSE", "END"])?;
            elsif_branches.push((elsif_cond, elsif_stmts));
        }

        let else_block = if self.try_keyword("ELSE") {
            Some(self.parse_statements_until(&["END"])?)
        } else {
            None
        };

        self.finish_end("IF", None)?;

        Ok(ProceduralStatement::If {
            condition,
            then_block,
            elsif_branches,
            else_block,
        })
    }

    /// Parse WHILE statement
    fn parse_while_statement(&mut self, label: Option<String>) -> Result<ProceduralStatement> {
        self.skip_whitespace();
        let condition = self.parse_expression(Stops::keywords(&["LOOP"]))?;

        self.skip_whitespace();
        self.expect_keyword("LOOP")?;

        let body = self.parse_statements_until(&["END"])?;
        self.finish_end("LOOP", label.as_deref())?;

        Ok(ProceduralStatement::While { label, condition, body })
    }

    /// Parse simple LOOP statement
    fn parse_loop_statement(&mut self, label: Option<String>) -> Result<ProceduralStatement> {
        let body = self.parse_statements_until(&["END"])?;
        self.finish_end("LOOP", label.as_deref())?;
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
        if !reverse && (self.check_keyword("SELECT") || self.check_keyword("WITH") || self.peek_char() == Some('(')) {
            // Query FOR
            let query = self.scan_fragment(Stops::keywords(&["LOOP"]));
            self.skip_whitespace();
            self.expect_keyword("LOOP")?;
            let body = self.parse_statements_until(&["END"])?;
            self.finish_end("LOOP", label.as_deref())?;

            Ok(ProceduralStatement::ForQuery {
                label,
                record_variable: variable,
                query,
                body,
            })
        } else {
            // Numeric FOR: `[REVERSE] first .. last [BY step] LOOP`
            let lower_bound = self.parse_expression(Stops {
                dotdot: true,
                ..Stops::default()
            })?;

            self.skip_whitespace();
            if !self.rest().starts_with("..") {
                return Err(Error::sql_parse(format!(
                    "Expected '..' in FOR loop at line {}, column {}",
                    self.line, self.col
                )));
            }
            self.advance_bytes(2);
            self.skip_whitespace();

            let upper_bound = self.parse_expression(Stops::keywords(&["LOOP", "BY"]))?;

            self.skip_whitespace();
            let step = if self.try_keyword("BY") {
                self.skip_whitespace();
                Some(self.parse_expression(Stops::keywords(&["LOOP"]))?)
            } else {
                None
            };

            self.skip_whitespace();
            self.expect_keyword("LOOP")?;

            let body = self.parse_statements_until(&["END"])?;
            self.finish_end("LOOP", label.as_deref())?;

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

    /// `EXIT|CONTINUE [label] [WHEN condition];` — the optional label and
    /// condition. `WHEN` is never a label (it used to be read as one, which
    /// dropped the condition and made `EXIT WHEN …` an unconditional exit).
    fn parse_label_and_when(&mut self) -> Result<(Option<String>, Option<PlExpr>)> {
        self.skip_whitespace();
        let label = if self.check_keyword("WHEN") || self.peek_char() == Some(';') {
            None
        } else {
            self.try_parse_identifier()
        };
        self.skip_whitespace();

        let when_condition = if self.try_keyword("WHEN") {
            self.skip_whitespace();
            Some(self.parse_expression(Stops::default())?)
        } else {
            None
        };

        self.eat_semicolon();
        Ok((label, when_condition))
    }

    /// Parse EXIT statement
    fn parse_exit_statement(&mut self) -> Result<ProceduralStatement> {
        let (label, when_condition) = self.parse_label_and_when()?;
        Ok(ProceduralStatement::Exit { label, when_condition })
    }

    /// Parse CONTINUE statement
    fn parse_continue_statement(&mut self) -> Result<ProceduralStatement> {
        let (label, when_condition) = self.parse_label_and_when()?;
        Ok(ProceduralStatement::Continue { label, when_condition })
    }

    /// Parse a CASE statement: searched (`CASE WHEN cond THEN …`) or simple
    /// (`CASE operand WHEN value[, value …] THEN …`), ended by `END CASE`.
    fn parse_case_statement(&mut self) -> Result<ProceduralStatement> {
        self.skip_whitespace();
        let operand = if self.check_keyword("WHEN") {
            None
        } else {
            Some(self.parse_expression(Stops::keywords(&["WHEN"]))?)
        };

        let mut when_branches = Vec::new();
        while self.try_keyword("WHEN") {
            self.skip_whitespace();
            let value = self.parse_expression(Stops::keywords(&["THEN"]))?;
            self.skip_whitespace();
            self.expect_keyword("THEN")?;
            let stmts = self.parse_statements_until(&["WHEN", "ELSE", "END"])?;
            when_branches.push((value, stmts));
        }
        if when_branches.is_empty() {
            return Err(Error::sql_parse(format!(
                "CASE statement needs at least one WHEN at line {}, column {}",
                self.line, self.col
            )));
        }

        let else_block = if self.try_keyword("ELSE") {
            Some(self.parse_statements_until(&["END"])?)
        } else {
            None
        };

        self.finish_end("CASE", None)?;

        Ok(match operand {
            None => ProceduralStatement::Case {
                when_branches,
                else_block,
            },
            Some(operand) => ProceduralStatement::SimpleCase {
                operand,
                when_branches,
                else_block,
            },
        })
    }

    /// Parse RETURN statement
    fn parse_return_statement(&mut self) -> Result<ProceduralStatement> {
        self.skip_whitespace();

        // Check for semicolon (RETURN without value)
        if self.peek_char() == Some(';') || self.at_eof() || self.check_keyword("END") {
            self.eat_semicolon();
            return Ok(ProceduralStatement::Return { value: None });
        }

        // Check for NEXT (RETURN NEXT)
        if self.try_keyword("NEXT") {
            self.skip_whitespace();
            let value = self.parse_text_literal(Stops::default());
            self.eat_semicolon();
            return Ok(ProceduralStatement::ReturnNext { value });
        }

        // Check for QUERY (RETURN QUERY)
        if self.try_keyword("QUERY") {
            self.skip_whitespace();
            let query = self.parse_until_semicolon();
            return Ok(ProceduralStatement::ReturnQuery { query });
        }

        // Regular RETURN with value
        let value = self.parse_expression(Stops::default())?;
        self.eat_semicolon();

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
        self.skip_whitespace();
        let message = if self.peek_char() != Some(';') && !self.check_keyword("USING") {
            Some(self.parse_text_literal(Stops::keywords(&["USING"])))
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
                    detail = Some(self.parse_text_literal(Stops {
                        comma: true,
                        ..Stops::default()
                    }));
                } else if self.try_keyword("HINT") {
                    self.skip_whitespace();
                    if self.peek_char() == Some('=') {
                        self.advance();
                        self.skip_whitespace();
                    }
                    hint = Some(self.parse_text_literal(Stops {
                        comma: true,
                        ..Stops::default()
                    }));
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

        self.eat_semicolon();

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
        let message = self.parse_text_literal(Stops::default());
        self.eat_semicolon();
        Ok(ProceduralStatement::Print { message })
    }

    /// Try to parse assignment or SQL execution
    fn try_parse_assignment_or_execute(&mut self) -> Result<Option<ProceduralStatement>> {
        let start = (self.pos, self.line, self.col);

        // Try to parse as assignment
        if let Some(target) = self.try_parse_identifier() {
            self.skip_whitespace();

            // `:=`, or a lone `=` (PostgreSQL accepts both for assignment).
            let rest = self.rest();
            let assign_len = if rest.starts_with(":=") {
                Some(2)
            } else if rest.starts_with('=') && !rest.starts_with("==") && !rest.starts_with("=>") {
                Some(1)
            } else {
                None
            };
            if let Some(n) = assign_len {
                self.advance_bytes(n);
                self.skip_whitespace();
                let value = self.parse_expression(Stops::default())?;
                self.eat_semicolon();
                return Ok(Some(ProceduralStatement::Assignment { target, value }));
            }

            // T-SQL style: SET @var = value. Only an `@` variable: a SQL
            // `SET search_path = public` is a statement, not an assignment.
            if target.eq_ignore_ascii_case("SET") && self.rest().starts_with('@') {
                if let Some(var_name) = self.try_parse_identifier() {
                    self.skip_whitespace();
                    if self.peek_char() == Some('=') {
                        self.advance();
                        self.skip_whitespace();
                        let value = self.parse_expression(Stops::default())?;
                        self.eat_semicolon();
                        return Ok(Some(ProceduralStatement::Assignment {
                            target: var_name,
                            value,
                        }));
                    }
                }
            }
        }

        // Reset and try as SQL execution
        (self.pos, self.line, self.col) = start;

        // Parse as SQL statement until its `;` (quote-aware: a `;` inside a
        // string literal does not end it).
        let sql = self.parse_until_semicolon();
        if sql.trim().is_empty() {
            return Ok(None);
        }

        // In a procedural body, `SELECT … INTO <var>[, …]` assigns into
        // VARIABLES — it is NEVER CTAS (PostgreSQL parity). The SQL layer now
        // treats a top-level `SELECT … INTO t` as `CREATE TABLE t AS …`, and
        // body statements are handed to the engine VERBATIM through the
        // `sql_executor` closure, so without this split a routine body like
        // `SELECT COUNT(*) INTO cnt FROM …` would silently create a junk table
        // named `cnt`. Splitting here also finally gives
        // `ProceduralStatement::SelectInto` a producer — the runtime arm has
        // always been implemented and never reachable.
        if let Some((query, variables)) = Self::split_select_into(&sql) {
            return Ok(Some(ProceduralStatement::SelectInto { query, variables }));
        }

        Ok(Some(ProceduralStatement::Execute {
            sql,
            into_variables: Vec::new(),
        }))
    }

    /// Split a body statement of the form
    /// `SELECT <items> INTO [STRICT] <var>[, …] <rest>` into the INTO-stripped
    /// query text and the target variable names.
    ///
    /// Confident-or-`None`: the statement's leading keyword must be `SELECT`,
    /// the `INTO` must sit at paren depth 0 outside quoted spans, and at least
    /// one plain identifier must follow it. So `INSERT INTO t …`, a nested
    /// `SELECT … FROM (SELECT … INTO …)`, `POSITION('x' IN s)` and an `INTO`
    /// inside a string literal are all left to the plain `Execute` path. A
    /// leading `WITH` is deliberately NOT accepted — `WITH … INSERT INTO t …`
    /// also carries a depth-0 `INTO`, and a wrong split is far worse here than
    /// a missed one.
    ///
    /// Record/composite INTO targets (`INTO rec` where `rec` is a ROWTYPE) are
    /// out of scope: the runtime assigns column-by-column into scalar names.
    #[allow(clippy::indexing_slicing)] // Byte cursor bounded by `while i < n`; slices are on ASCII boundaries.
    fn split_select_into(sql: &str) -> Option<(String, Vec<String>)> {
        let trimmed = sql.trim_start();
        if !crate::starts_with_icase(trimmed, "SELECT") {
            return None;
        }
        if trimmed
            .as_bytes()
            .get("SELECT".len())
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
        {
            return None;
        }

        let bytes = sql.as_bytes();
        let n = bytes.len();
        let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';

        // Locate the first depth-0, unquoted, whole-word `INTO`.
        let mut depth = 0i32;
        let mut i = 0usize;
        let into_start = loop {
            if i >= n {
                return None;
            }
            let c = bytes[i];
            if c == b'\'' || c == b'"' {
                let quote = c;
                i += 1;
                while i < n {
                    if bytes[i] == quote {
                        // A doubled quote is an embedded one — stay inside.
                        if i + 1 < n && bytes[i + 1] == quote {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            if c == b'(' {
                depth += 1;
                i += 1;
                continue;
            }
            if c == b')' {
                depth -= 1;
                i += 1;
                continue;
            }
            if depth == 0
                && i + 4 <= n
                && bytes[i..i + 4].eq_ignore_ascii_case(b"INTO")
                && (i == 0 || !is_ident(bytes[i - 1]))
                && (i + 4 == n || !is_ident(bytes[i + 4]))
            {
                break i;
            }
            i += 1;
        };

        // `INTO STRICT v` (PL/pgSQL): the STRICT row-count check is not
        // modelled, but the keyword must still be consumed so `STRICT` is not
        // mistaken for the first target variable.
        let mut rest = sql[into_start + 4..].trim_start();
        if let Some(after_strict) = Self::strip_leading_word(rest, "STRICT") {
            rest = after_strict;
        }

        let mut variables = Vec::new();
        loop {
            let name_len = rest.as_bytes().iter().take_while(|b| is_ident(**b)).count();
            if name_len == 0 || rest.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
                return None;
            }
            // A non-ASCII byte immediately after the name means this identifier
            // is not one the scanner understands — bail rather than split it in
            // the middle and emit a mangled query.
            if rest.as_bytes().get(name_len).is_some_and(|b| !b.is_ascii()) {
                return None;
            }
            variables.push(rest[..name_len].to_string());
            rest = rest[name_len..].trim_start();
            let Some(after_comma) = rest.strip_prefix(',') else {
                break;
            };
            rest = after_comma.trim_start();
        }

        // Rebuild the query without the INTO clause, keeping the tail
        // (`FROM …`, `WHERE …`) intact.
        let mut query = sql[..into_start].trim_end().to_string();
        if !rest.is_empty() {
            query.push(' ');
            query.push_str(rest);
        }
        Some((query, variables))
    }

    /// Strip a leading ASCII keyword (case-insensitive, right word boundary)
    /// and return the remainder with leading whitespace trimmed.
    #[allow(clippy::indexing_slicing)] // `starts_with_icase` proved the ASCII prefix.
    fn strip_leading_word<'a>(s: &'a str, word: &str) -> Option<&'a str> {
        if !crate::starts_with_icase(s, word) {
            return None;
        }
        let rest = &s[word.len()..];
        match rest.chars().next() {
            Some(c) if c.is_alphanumeric() || c == '_' => None,
            _ => Some(rest.trim_start()),
        }
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
            if self.at_eof() {
                break;
            }

            // Check for terminating keyword
            for kw in keywords {
                if self.check_keyword(kw) {
                    return Ok(statements);
                }
            }

            let before = self.pos;
            match self.try_parse_statement()? {
                Some(stmt) => statements.push(stmt),
                None => break,
            }
            // A statement that consumed nothing (a stray `)`) would loop forever.
            if self.pos == before {
                return Err(Error::sql_parse(format!(
                    "unexpected input at line {}, column {}",
                    self.line, self.col
                )));
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
                Some(self.parse_text_literal(Stops {
                    comma: true,
                    ..Stops::default()
                }))
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
            return self.parse_string_literal();
        }

        Err(Error::sql_parse("Expected function body delimiter"))
    }

    fn parse_label(&mut self) -> Result<String> {
        if !self.rest().starts_with("<<") {
            return Err(Error::sql_parse("Expected label <<name>>"));
        }
        self.advance_bytes(2);
        let end = self
            .rest()
            .find(">>")
            .ok_or_else(|| Error::sql_parse("Unterminated label <<name>>"))?;
        let label = self.rest().get(..end).unwrap_or("").trim().to_string();
        self.advance_bytes(end + 2);
        Ok(label)
    }

    fn parse_identifier(&mut self) -> Result<String> {
        self.try_parse_identifier().ok_or_else(|| {
            Error::sql_parse(format!(
                "Expected identifier at line {}, column {}",
                self.line, self.col
            ))
        })
    }

    fn try_parse_identifier(&mut self) -> Option<String> {
        self.skip_whitespace();
        let rest = self.rest();

        // Quoted identifier ("" escapes a quote)
        if rest.starts_with('"') {
            let len = quoted_len(rest, false)?;
            let raw = rest.get(1..len.saturating_sub(1)).unwrap_or("").replace("\"\"", "\"");
            self.advance_bytes(len);
            return Some(raw);
        }

        // T-SQL @variable
        let at = usize::from(rest.starts_with('@'));
        let body = rest.get(at..).unwrap_or("");
        if !body.chars().next().is_some_and(is_ident_start) {
            return None;
        }
        let len = at
            + body
                .char_indices()
                .find(|&(_, c)| !(c.is_alphanumeric() || c == '_'))
                .map_or(body.len(), |(i, _)| i);
        let ident = rest.get(..len)?.to_string();
        self.advance_bytes(len);
        Some(ident)
    }

    /// Scan one procedural expression, kept as SQL text (see [`PlExpr`]).
    fn parse_expression(&mut self, stops: Stops) -> Result<PlExpr> {
        let text = self.scan_fragment(stops);
        if text.is_empty() {
            return Err(Error::sql_parse(format!(
                "Expected an expression at line {}, column {}",
                self.line, self.col
            )));
        }
        Ok(PlExpr::new(text))
    }

    /// The text of a fragment, wrapped as the string literal the statements
    /// that still carry a `LogicalExpr` (RAISE, PRINT, RETURN NEXT, parameter
    /// defaults) have always received.
    fn parse_text_literal(&mut self, stops: Stops) -> LogicalExpr {
        LogicalExpr::Literal(crate::Value::String(self.scan_fragment(stops)))
    }

    /// Scan an embedded SQL fragment up to (not including) its terminator and
    /// return it trimmed. String literals, quoted identifiers, dollar quotes
    /// and comments are skipped whole, parentheses nest, a `CASE … END` of the
    /// fragment itself is not ended by its `END`, and stop keywords match whole
    /// words only — so a literal containing `'THEN'` or a variable named
    /// `status_end` no longer cuts the expression short. See [`Stops`].
    fn scan_fragment(&mut self, stops: Stops) -> String {
        let start = self.pos;
        let mut depth = 0usize;
        let mut case_depth = 0usize;
        let mut prev: Option<char> = None;
        while let Some(c) = self.peek_char() {
            let rest = self.rest();
            if let Some(n) = quoted_len(rest, matches!(prev, Some('e' | 'E'))) {
                self.advance_bytes(n);
                prev = Some(' ');
                continue;
            }
            match c {
                '(' | '[' => depth += 1,
                ')' | ']' => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                ';' if depth == 0 => break,
                ',' if depth == 0 && case_depth == 0 && stops.comma => break,
                '.' if depth == 0 && stops.dotdot && rest.starts_with("..") => break,
                c if is_ident_start(c) && !prev.is_some_and(is_ident_char) => {
                    let len = rest
                        .char_indices()
                        .find(|&(_, ch)| !is_ident_char(ch))
                        .map_or(rest.len(), |(i, _)| i);
                    let word = rest.get(..len).unwrap_or("");
                    if depth == 0 {
                        if word.eq_ignore_ascii_case("CASE") {
                            case_depth += 1;
                        } else if word.eq_ignore_ascii_case("END") && case_depth > 0 {
                            case_depth -= 1;
                        } else if case_depth == 0 && stops.keywords.iter().any(|k| word.eq_ignore_ascii_case(k)) {
                            break;
                        }
                    }
                    let last = word.chars().last();
                    self.advance_bytes(len);
                    prev = last;
                    continue;
                }
                _ => {}
            }
            self.advance();
            prev = Some(c);
        }
        self.source.get(start..self.pos).unwrap_or("").trim().to_string()
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
            "DOUBLE" | "FLOAT8" => {
                self.try_keyword("PRECISION");
                DataType::Float8
            }
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

        // A type modifier the engine does not model (`NUMERIC(10, 2)`,
        // `TIMESTAMP(3)`) is consumed whole, so it is not read as a default.
        if self.rest().starts_with('(') {
            if let Some(close) = self.rest().find(')') {
                self.advance_bytes(close + 1);
            }
        }

        // Handle array
        if is_array {
            Some(DataType::Array(Box::new(base_type)))
        } else {
            Some(base_type)
        }
    }

    /// A single-quoted SQL string literal, unescaped (`''` → `'`).
    fn parse_string_literal(&mut self) -> Result<String> {
        self.skip_whitespace();
        let rest = self.rest();
        if !rest.starts_with('\'') {
            return Err(Error::sql_parse(format!(
                "Expected string literal at line {}, column {}",
                self.line, self.col
            )));
        }
        let len = quoted_len(rest, false).unwrap_or(rest.len());
        if len < 2 || !rest.get(..len).is_some_and(|s| s.ends_with('\'')) {
            return Err(Error::sql_parse("Unterminated string literal"));
        }
        let body = rest.get(1..len - 1).unwrap_or("").replace("''", "'");
        self.advance_bytes(len);
        Ok(body)
    }

    /// A SQL statement up to its `;` (quote-aware), consuming the `;`.
    fn parse_until_semicolon(&mut self) -> String {
        let sql = self.scan_fragment(Stops::default());
        self.eat_semicolon();
        sql
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

    fn eat_semicolon(&mut self) {
        self.skip_whitespace();
        if self.peek_char() == Some(';') {
            self.advance();
        }
    }

    fn skip_whitespace(&mut self) {
        loop {
            let rest = self.rest();
            match rest.chars().next() {
                Some(c) if c.is_whitespace() => self.advance(),
                Some('-') if rest.starts_with("--") => {
                    let n = rest.find('\n').unwrap_or(rest.len());
                    self.advance_bytes(n);
                }
                Some('/') if rest.starts_with("/*") => {
                    let n = rest.find("*/").map_or(rest.len(), |e| e + 2);
                    self.advance_bytes(n);
                }
                _ => break,
            }
        }
    }

    /// The unparsed remainder of the source. Positions are BYTE offsets.
    fn rest(&self) -> &str {
        self.source.get(self.pos..).unwrap_or("")
    }

    fn at_eof(&self) -> bool {
        self.pos >= self.source.len()
    }

    fn peek_char(&self) -> Option<char> {
        self.rest().chars().next()
    }

    /// Advance one character.
    fn advance(&mut self) {
        if let Some(c) = self.peek_char() {
            self.pos += c.len_utf8();
            if c == '\n' {
                self.line += 1;
                self.col = 1;
            } else {
                self.col += 1;
            }
        }
    }

    /// Advance `n` bytes (a whole number of characters), keeping line/column.
    fn advance_bytes(&mut self, n: usize) {
        let end = (self.pos + n).min(self.source.len());
        let chunk = self.source.get(self.pos..end).unwrap_or("");
        for c in chunk.chars() {
            if c == '\n' {
                self.line += 1;
                self.col = 1;
            } else {
                self.col += 1;
            }
        }
        self.pos = end;
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
        if self.try_keyword(keyword) {
            Ok(())
        } else {
            Err(Error::sql_parse(format!(
                "Expected '{}' at line {}, column {}",
                keyword, self.line, self.col
            )))
        }
    }

    fn try_keyword(&mut self, keyword: &str) -> bool {
        self.skip_whitespace();
        if starts_with_keyword(self.rest(), keyword) {
            self.advance_bytes(keyword.len());
            true
        } else {
            false
        }
    }

    fn check_keyword(&mut self, keyword: &str) -> bool {
        self.skip_whitespace();
        starts_with_keyword(self.rest(), keyword)
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
            ProceduralStatement::If {
                then_block, else_block, ..
            } => {
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
