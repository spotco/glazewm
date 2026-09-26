use anyhow::Context;

/// Quote a single IPC argument so whitespace survives quote-aware splitting.
///
/// Arguments without whitespace or double-quotes are left unquoted. Otherwise
/// the value is wrapped in double quotes; embedded `"` are backslash-escaped.
/// Backslashes are left literal (Windows paths) except for the `\"` escape.
///
/// Empty strings become `""` so a distinct empty argument survives the
/// split/join round-trip.
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

/// Reconstruct an argv list after quote-aware splitting.
///
/// `split_ipc_args` strips grouping quotes from tokens. Joining with a bare
/// space would drop them (e.g. `shell-exec … "C:\My Scripts\foo.ps1"`
/// becomes an unquoted multi-token path). Re-apply [`quote_ipc_arg`] so
/// `ShellExec` / `parse_command` see the same quoting semantics as the
/// original IPC/config message. Layout path commands keep using quote-aware
/// split; only the reconstruct-for-exec path needs this join.
#[must_use]
pub fn join_ipc_args(args: &[impl AsRef<str>]) -> String {
  args
    .iter()
    .map(|a| quote_ipc_arg(a.as_ref()))
    .collect::<Vec<_>>()
    .join(" ")
}

/// Split an IPC message into argv tokens, honouring double-quoted segments.
///
/// Unquoted whitespace separates arguments. Inside double quotes, `\"` is an escaped literal quote;
/// other backslashes stay literal (Windows paths). Unclosed quotes are an error.
///
/// An empty quoted argument (`""`) yields one empty string token.
pub fn split_ipc_args(message: &str) -> anyhow::Result<Vec<String>> {
  let mut args = Vec::new();
  let mut current = String::new();
  let mut chars = message.chars().peekable();
  let mut in_quotes = false;
  // Tracks whether the current argv slot was opened (including via `""`),
  // so an empty quoted argument is preserved instead of discarded.
  let mut arg_started = false;

  while let Some(c) = chars.next() {
    match c {
      '"' if in_quotes => {
        in_quotes = false;
      }
      '"' => {
        in_quotes = true;
        arg_started = true;
      }
      '\\' if in_quotes => match chars.peek().copied() {
        Some('"') => {
          current.push(chars.next().expect("peeked"));
        }
        _ => current.push('\\'),
      },
      c if c.is_whitespace() && !in_quotes => {
        if arg_started {
          args.push(std::mem::take(&mut current));
          arg_started = false;
        }
      }
      _ => {
        current.push(c);
        arg_started = true;
      }
    }
  }

  if in_quotes {
    anyhow::bail!("Unclosed double-quote in IPC message.");
  }

  if arg_started {
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
  fn quote_empty_arg_is_quoted_empty() {
    assert_eq!(quote_ipc_arg(""), "\"\"");
  }

  #[test]
  fn split_empty_quoted_argument() {
    let tokens = split_ipc_args("\"\"").unwrap();
    assert_eq!(tokens, vec![String::new()]);
  }

  #[test]
  fn round_trip_empty_arg_quote_then_split() {
    let quoted = quote_ipc_arg("");
    assert_eq!(quoted, "\"\"");
    let tokens = split_ipc_args(&quoted).unwrap();
    assert_eq!(tokens, vec![String::new()]);
  }

  #[test]
  fn round_trip_empty_arg_among_others() {
    let args = ["powershell".to_string(), String::new(), "-File".to_string()];
    let joined = join_ipc_args(&args);
    assert_eq!(joined, "powershell \"\" -File");
    let tokens = split_ipc_args(&joined).unwrap();
    assert_eq!(tokens, args);
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

  #[test]
  fn shell_exec_powershell_file_path_with_spaces() {
    let msg = r#"shell-exec powershell -File "C:\My Scripts\foo.ps1""#;
    let tokens = split_ipc_args(msg).unwrap();
    assert_eq!(
      tokens,
      vec![
        "shell-exec".to_string(),
        "powershell".to_string(),
        "-File".to_string(),
        r"C:\My Scripts\foo.ps1".to_string(),
      ]
    );
    // Reconstruct the command tail the way ShellExec joins for exec.
    let reconstructed = join_ipc_args(&tokens[1..]);
    assert_eq!(
      reconstructed,
      r#"powershell -File "C:\My Scripts\foo.ps1""#
    );
    let reparsed = split_ipc_args(&reconstructed).unwrap();
    assert_eq!(reparsed, tokens[1..]);
  }

  #[test]
  fn shell_exec_code_project_path_with_spaces() {
    let msg = r#"shell-exec code "C:\My Project""#;
    let tokens = split_ipc_args(msg).unwrap();
    assert_eq!(
      tokens,
      vec![
        "shell-exec".to_string(),
        "code".to_string(),
        r"C:\My Project".to_string(),
      ]
    );
    assert_eq!(
      join_ipc_args(&tokens[1..]),
      r#"code "C:\My Project""#
    );
  }

  #[test]
  fn shell_exec_multiple_separately_quoted_args() {
    let msg = r#"shell-exec prog "arg one" "arg two" plain"#;
    let tokens = split_ipc_args(msg).unwrap();
    assert_eq!(
      tokens,
      vec![
        "shell-exec".to_string(),
        "prog".to_string(),
        "arg one".to_string(),
        "arg two".to_string(),
        "plain".to_string(),
      ]
    );
    assert_eq!(
      join_ipc_args(&tokens[1..]),
      r#"prog "arg one" "arg two" plain"#
    );
  }

  #[test]
  fn shell_exec_embedded_escaped_quotes() {
    let msg = r#"shell-exec echo "say \"hello\"""#;
    let tokens = split_ipc_args(msg).unwrap();
    assert_eq!(tokens.len(), 3);
    assert_eq!(tokens[2], r#"say "hello""#);
    let joined = join_ipc_args(&tokens[1..]);
    let again = split_ipc_args(&joined).unwrap();
    assert_eq!(again, tokens[1..]);
  }

  #[test]
  fn shell_exec_unc_path_with_spaces() {
    let unc = r"\\server\share\My Folder\file.ps1";
    let msg = format!(
      "shell-exec powershell -File {}",
      quote_ipc_arg(unc)
    );
    let tokens = split_ipc_args(&msg).unwrap();
    assert_eq!(tokens.last().map(String::as_str), Some(unc));
    let reconstructed = join_ipc_args(&tokens[1..]);
    assert!(reconstructed.contains('"'));
    let reparsed = split_ipc_args(&reconstructed).unwrap();
    assert_eq!(reparsed.last().map(String::as_str), Some(unc));
  }

  #[test]
  fn shell_exec_empty_quoted_argument_in_command() {
    let msg = r#"shell-exec tool "" --flag"#;
    let tokens = split_ipc_args(msg).unwrap();
    assert_eq!(
      tokens,
      vec![
        "shell-exec".to_string(),
        "tool".to_string(),
        String::new(),
        "--flag".to_string(),
      ]
    );
    assert_eq!(join_ipc_args(&tokens[1..]), r#"tool "" --flag"#);
  }
}
