use anyhow::Context;

/// Quote a single IPC argument so whitespace survives quote-aware splitting.
///
/// Arguments without whitespace or double-quotes are left unquoted. Otherwise
/// the value is wrapped in double quotes; embedded `"` are backslash-escaped.
/// Backslashes are left literal (Windows paths) except for the `\"` escape.
#[must_use]
pub fn quote_ipc_arg(arg: &str) -> String {
  if arg.is_empty() || arg.chars().any(|c| c.is_whitespace() || c == '"') {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
      if c == '"' {
        out.push('\\');
      }
      out.push(c);
    }
    out.push('"');
    out
  } else {
    arg.to_string()
  }
}

/// Split an IPC message into argv tokens, honouring double-quoted segments.
///
/// Unquoted whitespace separates arguments. Inside double quotes, `\"` and
/// `\\` are treated as escaped literals. Unclosed quotes are an error.
pub fn split_ipc_args(message: &str) -> anyhow::Result<Vec<String>> {
  let mut args = Vec::new();
  let mut current = String::new();
  let mut chars = message.chars().peekable();
  let mut in_quotes = false;

  while let Some(c) = chars.next() {
    match c {
      '"' if in_quotes => {
        in_quotes = false;
      }
      '"' => {
        in_quotes = true;
      }
      '\\' if in_quotes => match chars.peek().copied() {
        Some('"') | Some('\\') => {
          current.push(chars.next().expect("peeked"));
        }
        _ => current.push('\\'),
      },
      c if c.is_whitespace() && !in_quotes => {
        if !current.is_empty() {
          args.push(std::mem::take(&mut current));
        }
      }
      _ => current.push(c),
    }
  }

  if in_quotes {
    anyhow::bail!("Unclosed double-quote in IPC message.");
  }

  if !current.is_empty() {
    args.push(current);
  }

  Ok(args)
}

/// Parse `AppCommand`-compatible argv from an IPC message string.
///
/// Prepends an empty binary name (clap convention) after quote-aware split.
pub fn ipc_argv_from_message(
  message: &str,
) -> anyhow::Result<Vec<String>> {
  let mut args = split_ipc_args(message)
    .with_context(|| format!("Failed to tokenize IPC message: {message}"))?;
  args.insert(0, String::new());
  Ok(args)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::Path;

  #[test]
  fn quote_leaves_simple_paths_alone() {
    assert_eq!(quote_ipc_arg(r"C:\temp\layout.json"), r"C:\temp\layout.json");
  }

  #[test]
  fn quote_wraps_paths_with_spaces() {
    let path = r"C:\Users\mooto\AppData\Local\Temp\glazewm path test\a.json";
    let quoted = quote_ipc_arg(path);
    assert_eq!(quoted, format!("\"{path}\""));
    let tokens = split_ipc_args(&format!("load-layout {quoted}")).unwrap();
    assert_eq!(tokens, vec!["load-layout".to_string(), path.to_string()]);
  }

  #[test]
  fn round_trip_all_path_bearing_commands() {
    let path = r"D:\my layouts\snap shot.json";
    let quoted = quote_ipc_arg(path);
    let messages = [
      format!("load-layout {quoted}"),
      format!("inspect-layout {quoted}"),
      format!("query layout-match {quoted}"),
      format!("command load-layout {quoted}"),
    ];
    for msg in messages {
      let tokens = split_ipc_args(&msg).unwrap();
      assert_eq!(tokens.last().map(String::as_str), Some(path));
    }
  }

  #[test]
  fn path_display_helper_matches_quote_ipc_arg() {
    let p = Path::new(r"C:\Temp\glazewm path test\file.json");
    assert_eq!(
      quote_ipc_arg(&p.display().to_string()),
      format!("\"{}\"", p.display())
    );
  }

  #[test]
  fn unclosed_quote_errors() {
    assert!(split_ipc_args(r#"load-layout "C:\broken"#).is_err());
  }

  #[test]
  fn escaped_quote_inside_path() {
    let tokens =
      split_ipc_args(r#"inspect-layout "C:\odd\"name\file.json""#).unwrap();
    assert_eq!(tokens.len(), 2);
    assert_eq!(tokens[1], r#"C:\odd"name\file.json"#);
  }

  #[test]
  fn ipc_argv_prepends_empty_bin() {
    let argv = ipc_argv_from_message(r#"load-layout "C:\a b\c.json""#).unwrap();
    assert_eq!(argv[0], "");
    assert_eq!(argv[1], "load-layout");
    assert_eq!(argv[2], r"C:\a b\c.json");
  }
}
