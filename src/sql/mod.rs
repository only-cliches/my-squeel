pub mod engine;

use sqlparser::ast::Statement;
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;

pub fn parse(sql: &str) -> Result<Vec<Statement>, sqlparser::parser::ParserError> {
    match Parser::parse_sql(&MySqlDialect {}, sql) {
        Ok(statements) => Ok(statements),
        Err(err) => {
            if let Some(rewritten) =
                rewrite_user_variable_assignments(sql).or_else(|| rewrite_drop_index_on_table(sql))
            {
                Parser::parse_sql(&MySqlDialect {}, &rewritten)
            } else {
                Err(err)
            }
        }
    }
}

/// sqlparser does not accept MySQL's expression assignment operator (`:=`)
/// in every expression context. Rewrite it to an internal function so the
/// evaluator can apply the assignment with normal expression semantics.
fn rewrite_user_variable_assignments(sql: &str) -> Option<String> {
    if !sql.contains(":=") {
        return None;
    }

    let bytes = sql.as_bytes();
    let mut output = String::with_capacity(sql.len() + 32);
    let mut index = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut changed = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'\'' && !in_double {
            output.push(byte as char);
            if index + 1 < bytes.len() && bytes[index + 1] == b'\'' {
                output.push('\'');
                index += 2;
                continue;
            }
            in_single = !in_single;
            index += 1;
            continue;
        }
        if byte == b'"' && !in_single {
            in_double = !in_double;
            output.push(byte as char);
            index += 1;
            continue;
        }
        if !in_single
            && !in_double
            && byte == b'@'
            && let Some(name_end) = (index + 1..bytes.len()).find(|position| {
                !bytes[*position].is_ascii_alphanumeric()
                    && bytes[*position] != b'_'
                    && bytes[*position] != b'$'
            })
        {
            let mut operator = name_end;
            while operator < bytes.len() && bytes[operator].is_ascii_whitespace() {
                operator += 1;
            }
            if bytes.get(operator..operator + 2) == Some(b":=") {
                let name = &sql[index + 1..name_end];
                output.push_str("USER_VAR_ASSIGN('");
                output.push_str(name.replace('\'', "''").as_str());
                output.push_str("', ");
                index = operator + 2;
                let mut depth = 0_u32;
                let mut rhs_single = false;
                let mut rhs_double = false;
                while index < bytes.len() {
                    let rhs_byte = bytes[index];
                    if rhs_byte == b'\'' && !rhs_double {
                        rhs_single = !rhs_single;
                    } else if rhs_byte == b'"' && !rhs_single {
                        rhs_double = !rhs_double;
                    } else if !rhs_single && !rhs_double {
                        if rhs_byte == b'(' {
                            depth += 1;
                        } else if rhs_byte == b')' {
                            if depth == 0 {
                                break;
                            }
                            depth -= 1;
                        } else if rhs_byte == b',' && depth == 0 {
                            break;
                        }
                    }
                    output.push(rhs_byte as char);
                    index += 1;
                }
                output.push(')');
                changed = true;
                continue;
            }
        }
        output.push(byte as char);
        index += 1;
    }
    changed.then_some(output)
}

fn rewrite_drop_index_on_table(sql: &str) -> Option<String> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let tokens = trimmed.split_whitespace().collect::<Vec<_>>();
    if tokens.len() >= 5
        && tokens[0].eq_ignore_ascii_case("DROP")
        && tokens[1].eq_ignore_ascii_case("INDEX")
    {
        let (name, on_pos) = if tokens[2].eq_ignore_ascii_case("IF")
            && tokens
                .get(3)
                .is_some_and(|token| token.eq_ignore_ascii_case("EXISTS"))
        {
            (tokens.get(4)?, 5)
        } else {
            (tokens.get(2)?, 3)
        };
        if tokens
            .get(on_pos)
            .is_some_and(|token| token.eq_ignore_ascii_case("ON"))
        {
            let if_exists = tokens[2].eq_ignore_ascii_case("IF");
            return Some(if if_exists {
                format!("DROP INDEX IF EXISTS {name}")
            } else {
                format!("DROP INDEX {name}")
            });
        }
    }
    None
}
