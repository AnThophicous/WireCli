#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

/// Splits a shell-style command line into argv without invoking a shell.
///
/// Supports whitespace separation, single quotes, double quotes, backslash
/// escapes, and empty quoted arguments. Operators such as `;`, `|`, or `$()`
/// are returned as plain argument text so the existing command policy can
/// decide whether to block them.
pub fn split_command_line(input: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut in_token = false;

    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            in_token = true;
            continue;
        }

        match quote {
            Quote::None => match ch {
                '\\' => {
                    escaped = true;
                    in_token = true;
                }
                '\'' => {
                    quote = Quote::Single;
                    in_token = true;
                }
                '"' => {
                    quote = Quote::Double;
                    in_token = true;
                }
                ch if ch.is_whitespace() => {
                    if in_token {
                        args.push(std::mem::take(&mut current));
                        in_token = false;
                    }
                }
                _ => {
                    current.push(ch);
                    in_token = true;
                }
            },
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    current.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '\\' => {
                    escaped = true;
                    in_token = true;
                }
                _ => current.push(ch),
            },
        }
    }

    if escaped {
        return Err("unterminated escape sequence".to_string());
    }

    if quote != Quote::None {
        return Err("unterminated quoted string".to_string());
    }

    if in_token {
        args.push(current);
    }

    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::split_command_line;

    #[test]
    fn keeps_quoted_arguments_together() {
        assert_eq!(
            split_command_line(r#"git commit -m "message with spaces""#).unwrap(),
            vec!["git", "commit", "-m", "message with spaces"]
        );
    }

    #[test]
    fn keeps_empty_quoted_arguments() {
        assert_eq!(
            split_command_line("printf \"%s\" \"\"").unwrap(),
            vec!["printf", "%s", ""]
        );
    }

    #[test]
    fn reports_unclosed_quotes() {
        assert!(split_command_line(r#"echo "unfinished"#).is_err());
    }
}
