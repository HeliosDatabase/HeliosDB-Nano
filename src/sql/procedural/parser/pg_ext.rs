//! The PL/pgSQL statements that are read from their whole text: `RAISE`,
//! `GET DIAGNOSTICS`, `EXECUTE` (dynamic SQL), `RETURNING … INTO`, and the
//! declaration types that name no scalar type (`RECORD`, `%ROWTYPE`, `%TYPE`).

use super::ProceduralParser;
use crate::sql::procedural::ast::{PlExpr, ProceduralDialect, ProceduralStatement, RaiseLevel, ROW_COUNT_EXPR};
use crate::sql::procedural::conditions::condition_sqlstate;
use crate::sql::sql_scan::{self, Tok, TokKind};
use crate::{Error, Result};

fn syntax(message: impl Into<String>) -> Error {
    Error::sql_state("42601", message)
}

/// The text the tokens span, trimmed.
fn span<'a>(text: &'a str, toks: &[Tok]) -> &'a str {
    match (toks.first(), toks.last()) {
        (Some(first), Some(last)) => text.get(first.start..last.end).unwrap_or("").trim(),
        _ => "",
    }
}

/// Split on the commas outside parentheses.
fn split_commas<'a>(text: &'a str, toks: &[Tok]) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut from = 0usize;
    for (i, tok) in toks.iter().enumerate() {
        if tok.depth == 0 && tok.is_punct(text, ',') {
            parts.push(span(text, toks.get(from..i).unwrap_or(&[])));
            from = i + 1;
        }
    }
    parts.push(span(text, toks.get(from..).unwrap_or(&[])));
    parts
}

/// The value of a plain `'…'` literal.
fn string_value(tok: &Tok, text: &str, what: &str) -> Result<String> {
    let raw = tok.text(text);
    raw.strip_prefix('\'')
        .and_then(|r| r.strip_suffix('\''))
        .map(|inner| inner.replace("''", "'"))
        .ok_or_else(|| syntax(format!("{what} must be a plain quoted string")))
}

/// A list of variable names (`a, b, c`), as written.
fn target_names(text: &str, toks: &[Tok], what: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for part in split_commas(text, toks) {
        let part_toks = sql_scan::tokens(part);
        match part_toks.as_slice() {
            [one] if matches!(one.kind, TokKind::Word | TokKind::Quoted) => {
                names.push(one.text(part).trim_matches('"').to_string());
            }
            _ => return Err(syntax(format!("{what}: `{part}` is not a variable name"))),
        }
    }
    Ok(names)
}

/// `DML … RETURNING list INTO a, b`: the statement without its INTO clause
/// and the target variables. `None` for any other statement.
pub(super) fn split_returning_into(sql: &str) -> Result<Option<(String, Vec<String>)>> {
    let toks = sql_scan::tokens(sql);
    let Some(first) = toks.first() else {
        return Ok(None);
    };
    if !(first.is_word(sql, "INSERT") || first.is_word(sql, "UPDATE") || first.is_word(sql, "DELETE")) {
        return Ok(None);
    }
    let Some(returning) = toks.iter().position(|t| t.depth == 0 && t.is_word(sql, "RETURNING")) else {
        return Ok(None);
    };
    let Some(into) = toks
        .iter()
        .enumerate()
        .skip(returning + 1)
        .find(|(_, t)| t.depth == 0 && t.is_word(sql, "INTO"))
        .map(|(i, _)| i)
    else {
        return Ok(None);
    };
    let mut targets = toks.get(into + 1..).unwrap_or(&[]);
    if targets.last().is_some_and(|t| t.is_punct(sql, ';')) {
        targets = targets.get(..targets.len() - 1).unwrap_or(&[]);
    }
    if targets.first().is_some_and(|t| t.is_word(sql, "STRICT")) {
        return Err(Error::sql_state(
            "0A000",
            "RETURNING … INTO STRICT is not supported; use INTO and test FOUND",
        ));
    }
    let names = target_names(sql, targets, "RETURNING … INTO")?;
    let statement = toks
        .get(into)
        .and_then(|t| sql.get(..t.start))
        .unwrap_or(sql)
        .trim_end()
        .to_string();
    Ok(Some((statement, names)))
}

/// Does `SELECT … INTO STRICT …` say STRICT?
pub(super) fn select_into_is_strict(sql: &str) -> bool {
    let toks = sql_scan::tokens(sql);
    toks.iter().enumerate().any(|(i, t)| {
        t.depth == 0 && t.is_word(sql, "INTO") && toks.get(i + 1).is_some_and(|next| next.is_word(sql, "STRICT"))
    })
}

impl ProceduralParser {
    /// In a declaration, after the variable's name: skip a type that names no
    /// scalar type (`RECORD`, `table%ROWTYPE`, `table.column%TYPE`). `true`
    /// when one was there; the variable then has no declared type.
    pub(super) fn skip_untyped_declaration_type(&mut self) -> bool {
        let head = self.rest().split(';').next().unwrap_or("");
        let toks = sql_scan::tokens(head);
        let end = match toks.first() {
            Some(first) if first.is_word(head, "RECORD") => Some(first.end),
            Some(_) => toks.iter().take(8).enumerate().find_map(|(i, tok)| {
                let next = toks.get(i + 1)?;
                (tok.is_punct(head, '%') && (next.is_word(head, "ROWTYPE") || next.is_word(head, "TYPE")))
                    .then_some(next.end)
            }),
            None => None,
        };
        match end {
            Some(end) => {
                self.advance_bytes(end);
                true
            }
            None => false,
        }
    }

    /// `RAISE …;` after the keyword.
    pub(super) fn parse_pg_raise(&mut self) -> Result<ProceduralStatement> {
        let text = self.parse_until_semicolon();
        let toks = sql_scan::tokens(&text);
        let mut at = 0usize;
        let mut level = RaiseLevel::Exception;
        if let Some(first) = toks.first() {
            for (word, named) in [
                ("EXCEPTION", RaiseLevel::Exception),
                ("WARNING", RaiseLevel::Warning),
                ("NOTICE", RaiseLevel::Notice),
                ("INFO", RaiseLevel::Info),
                ("LOG", RaiseLevel::Log),
                ("DEBUG", RaiseLevel::Debug),
            ] {
                if first.is_word(&text, word) {
                    level = named;
                    at = 1;
                    break;
                }
            }
        }
        let using = toks
            .iter()
            .enumerate()
            .skip(at)
            .find(|(_, t)| t.depth == 0 && t.is_word(&text, "USING"))
            .map(|(i, _)| i);
        let head = toks.get(at..using.unwrap_or(toks.len())).unwrap_or(&[]);

        let mut condition = None;
        let mut format = None;
        let mut args = Vec::new();
        match head.first() {
            None => {}
            Some(first) if first.kind == TokKind::Str => {
                format = Some(string_value(first, &text, "the RAISE format")?);
                let rest = head.get(1..).unwrap_or(&[]);
                if let Some(comma) = rest.first() {
                    if !comma.is_punct(&text, ',') {
                        return Err(syntax("RAISE: a comma is expected after the format string"));
                    }
                    for arg in split_commas(&text, rest.get(1..).unwrap_or(&[])) {
                        if arg.is_empty() {
                            return Err(syntax("RAISE: an argument is missing"));
                        }
                        args.push(PlExpr::new(arg));
                    }
                }
            }
            Some(first) if first.is_word(&text, "SQLSTATE") => match head {
                [_, code] if code.kind == TokKind::Str => {
                    let code = string_value(code, &text, "the SQLSTATE of RAISE")?;
                    if code.len() != 5 || !code.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase()) {
                        return Err(syntax(format!("RAISE: invalid SQLSTATE code '{code}'")));
                    }
                    condition = Some(code);
                }
                _ => return Err(syntax("RAISE: SQLSTATE takes one quoted code")),
            },
            Some(first) if first.kind == TokKind::Word && head.len() == 1 => {
                let name = first.text(&text).to_ascii_lowercase();
                let code = condition_sqlstate(&name).ok_or_else(|| {
                    Error::sql_state("42704", format!("unrecognized exception condition \"{name}\""))
                })?;
                condition = Some(code.to_string());
            }
            Some(_) => {
                return Err(syntax(
                    "RAISE: a format string, a condition name or SQLSTATE 'code' is expected",
                ))
            }
        }

        let mut options = Vec::new();
        if let Some(using) = using {
            for part in split_commas(&text, toks.get(using + 1..).unwrap_or(&[])) {
                let Some((name, value)) = part.split_once('=') else {
                    return Err(syntax(format!("RAISE … USING: `{part}` is not `option = value`")));
                };
                let name = name.trim().trim_end_matches(':').trim().to_ascii_uppercase();
                if !matches!(name.as_str(), "MESSAGE" | "DETAIL" | "HINT" | "ERRCODE") {
                    return Err(Error::sql_state(
                        "0A000",
                        format!("RAISE … USING {name} is not supported (MESSAGE, DETAIL, HINT and ERRCODE are)"),
                    ));
                }
                if value.trim().is_empty() || options.iter().any(|(seen, _): &(String, PlExpr)| *seen == name) {
                    return Err(syntax(format!("RAISE … USING {name}: one value is expected")));
                }
                options.push((name, PlExpr::new(value.trim())));
            }
        }
        if format.is_some() && options.iter().any(|(name, _)| name == "MESSAGE") {
            return Err(syntax("RAISE: USING MESSAGE cannot be given with a format string"));
        }
        if condition.is_some() && options.iter().any(|(name, _)| name == "ERRCODE") {
            return Err(syntax("RAISE: USING ERRCODE cannot be given with a condition"));
        }
        Ok(ProceduralStatement::Raise {
            level,
            condition,
            format,
            args,
            options,
        })
    }

    /// `GET DIAGNOSTICS` and `EXECUTE` (dynamic SQL); `None` for anything else.
    pub(super) fn try_parse_pg_statement(&mut self) -> Result<Option<ProceduralStatement>> {
        if self.dialect == ProceduralDialect::TSql {
            return Ok(None);
        }
        if self.check_keyword("GET") {
            let save = (self.pos, self.line, self.col);
            let text = self.parse_until_semicolon();
            let toks = sql_scan::tokens(&text);
            let mut at = 1usize;
            if toks.get(at).is_some_and(|t| t.is_word(&text, "CURRENT")) {
                at += 1;
            }
            if !toks.get(at).is_some_and(|t| t.is_word(&text, "DIAGNOSTICS")) {
                (self.pos, self.line, self.col) = save;
                return Ok(None);
            }
            let target = toks
                .get(at + 1)
                .filter(|t| matches!(t.kind, TokKind::Word | TokKind::Quoted))
                .map(|t| t.text(&text).trim_matches('"').to_string())
                .ok_or_else(|| syntax("GET DIAGNOSTICS: a variable name is expected"))?;
            let mut item = at + 2;
            if toks.get(item).is_some_and(|t| t.is_punct(&text, ':')) {
                item += 1;
            }
            if !toks.get(item).is_some_and(|t| t.is_punct(&text, '=')) {
                return Err(syntax("GET DIAGNOSTICS: `variable = item` is expected"));
            }
            return match toks.get(item + 1..).unwrap_or(&[]) {
                [one] if one.is_word(&text, "ROW_COUNT") => Ok(Some(ProceduralStatement::Assignment {
                    target,
                    value: PlExpr::new(ROW_COUNT_EXPR),
                })),
                _ => Err(Error::sql_state(
                    "0A000",
                    "GET DIAGNOSTICS: only `variable = ROW_COUNT` is supported",
                )),
            };
        }
        if self.check_keyword("EXECUTE") {
            let text = self.parse_until_semicolon();
            let toks = sql_scan::tokens(&text);
            let clause = |word: &str| {
                toks.iter()
                    .enumerate()
                    .skip(1)
                    .find(|(_, t)| t.depth == 0 && t.is_word(&text, word))
                    .map(|(i, _)| i)
            };
            let into = clause("INTO");
            let using = clause("USING");
            let end = toks.len();
            let command_end = into.unwrap_or(end).min(using.unwrap_or(end));
            let command = span(&text, toks.get(1..command_end).unwrap_or(&[]));
            if command.is_empty() {
                return Err(syntax("EXECUTE: a command string is expected"));
            }
            let mut strict = false;
            let mut into_variables = Vec::new();
            if let Some(into) = into {
                let stop = using.filter(|u| *u > into).unwrap_or(end);
                let mut targets = toks.get(into + 1..stop).unwrap_or(&[]);
                if targets.first().is_some_and(|t| t.is_word(&text, "STRICT")) {
                    strict = true;
                    targets = targets.get(1..).unwrap_or(&[]);
                }
                into_variables = target_names(&text, targets, "EXECUTE … INTO")?;
            }
            let mut using_parameters = Vec::new();
            if let Some(using) = using {
                let stop = into.filter(|i| *i > using).unwrap_or(end);
                for value in split_commas(&text, toks.get(using + 1..stop).unwrap_or(&[])) {
                    if value.is_empty() {
                        return Err(syntax("EXECUTE … USING: a value is missing"));
                    }
                    using_parameters.push(PlExpr::new(value));
                }
            }
            return Ok(Some(ProceduralStatement::ExecuteDynamic {
                sql_expression: PlExpr::new(command),
                into_variables,
                using_parameters,
                strict,
            }));
        }
        Ok(None)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn one(body: &str) -> ProceduralStatement {
        let mut parser = ProceduralParser::new(body);
        let block = parser.parse_block().unwrap();
        assert_eq!(block.statements.len(), 1, "{:?}", block.statements);
        block.statements.into_iter().next().unwrap()
    }

    #[test]
    fn raise_forms_are_read() {
        match one("BEGIN RAISE NOTICE 'a % b %', x + 1, f(y, 2); END") {
            ProceduralStatement::Raise { level, format, args, .. } => {
                assert_eq!(level, RaiseLevel::Notice);
                assert_eq!(format.as_deref(), Some("a % b %"));
                assert_eq!(args, [PlExpr::new("x + 1"), PlExpr::new("f(y, 2)")]);
            }
            other => panic!("{other:?}"),
        }
        match one("BEGIN RAISE EXCEPTION 'it''s %', n USING ERRCODE = 'P0042', HINT = 'try ' || n, DETAIL := 'd'; END") {
            ProceduralStatement::Raise { format, options, .. } => {
                assert_eq!(format.as_deref(), Some("it's %"));
                let names: Vec<&str> = options.iter().map(|(n, _)| n.as_str()).collect();
                assert_eq!(names, ["ERRCODE", "HINT", "DETAIL"]);
                assert_eq!(options[1].1, PlExpr::new("'try ' || n"));
            }
            other => panic!("{other:?}"),
        }
        match one("BEGIN RAISE unique_violation USING MESSAGE = 'dup'; END") {
            ProceduralStatement::Raise { condition, format, .. } => {
                assert_eq!(condition.as_deref(), Some("23505"));
                assert!(format.is_none());
            }
            other => panic!("{other:?}"),
        }
        match one("BEGIN RAISE SQLSTATE 'AB123'; END") {
            ProceduralStatement::Raise { condition, .. } => assert_eq!(condition.as_deref(), Some("AB123")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            one("BEGIN RAISE; END"),
            ProceduralStatement::Raise {
                condition: None,
                format: None,
                ..
            }
        ));
        let mut bad = ProceduralParser::new("BEGIN RAISE no_such_condition; END");
        assert!(bad.parse_block().is_err());
    }

    #[test]
    fn get_diagnostics_execute_and_returning_into_are_read() {
        match one("BEGIN GET DIAGNOSTICS n = ROW_COUNT; END") {
            ProceduralStatement::Assignment { target, value } => {
                assert_eq!(target, "n");
                assert_eq!(value.text, ROW_COUNT_EXPR);
            }
            other => panic!("{other:?}"),
        }
        match one("BEGIN EXECUTE 'SELECT a FROM t WHERE id = $1 AND s = $2' INTO STRICT v USING k, 'x, y'; END") {
            ProceduralStatement::ExecuteDynamic {
                sql_expression,
                into_variables,
                using_parameters,
                strict,
            } => {
                assert_eq!(sql_expression.text, "'SELECT a FROM t WHERE id = $1 AND s = $2'");
                assert_eq!(into_variables, ["v"]);
                assert_eq!(using_parameters, [PlExpr::new("k"), PlExpr::new("'x, y'")]);
                assert!(strict);
            }
            other => panic!("{other:?}"),
        }
        match one("BEGIN EXECUTE 'DELETE FROM ' || tbl USING a INTO n; END") {
            ProceduralStatement::ExecuteDynamic {
                sql_expression,
                into_variables,
                using_parameters,
                strict,
            } => {
                assert_eq!(sql_expression.text, "'DELETE FROM ' || tbl");
                assert_eq!(into_variables, ["n"]);
                assert_eq!(using_parameters, [PlExpr::new("a")]);
                assert!(!strict);
            }
            other => panic!("{other:?}"),
        }
        match one("BEGIN INSERT INTO t (v) VALUES (x) RETURNING id, v INTO new_id, new_v; END") {
            ProceduralStatement::Execute { sql, into_variables } => {
                assert_eq!(sql, "INSERT INTO t (v) VALUES (x) RETURNING id, v");
                assert_eq!(into_variables, ["new_id", "new_v"]);
            }
            other => panic!("{other:?}"),
        }
        match one("BEGIN SELECT a INTO STRICT v FROM t; END") {
            ProceduralStatement::SelectInto { strict, variables, .. } => {
                assert!(strict);
                assert_eq!(variables, ["v"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn record_and_copied_types_declare_untyped_variables() {
        let mut parser = ProceduralParser::new(
            "DECLARE r RECORD; row_of t%ROWTYPE; like_col public.t.id%TYPE; n INT := 3; BEGIN NULL; END",
        );
        let block = parser.parse_block().unwrap();
        let names: Vec<&str> = block.declarations.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["r", "row_of", "like_col", "n"]);
        assert!(block.declarations[0].data_type.is_none());
        assert!(block.declarations[1].data_type.is_none());
        assert!(block.declarations[2].data_type.is_none());
        assert!(block.declarations[3].data_type.is_some());
    }
}
