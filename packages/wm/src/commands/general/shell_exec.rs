use std::path::Path;

#[cfg(target_os = "windows")]
use anyhow::Context;
#[cfg(target_os = "macos")]
use shell_util::{CommandOptions, Shell};
#[cfg(target_os = "windows")]
use wm_platform::DispatcherExtWindows;

use crate::wm_state::WmState;

pub fn shell_exec(
  command: &str,
  // LINT: `hide_window` is only used on Windows.
  #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
  hide_window: bool,
  state: &WmState,
) -> anyhow::Result<()> {
  let (program, args) = parse_command(command, state)?;
  validate_shell_program(&program, command)?;
  tracing::info!(
    "Parsed command program: '{}', args: '{}'.",
    program,
    args
  );

  // NOTE: The standard library's `Command::new` is not used because it
  // launches the program as a subprocess. This prevents cleanup of handles
  // held by our process (e.g. the IPC server port) until the subprocess
  // exits.
  let result = {
    #[cfg(target_os = "macos")]
    {
      Shell::spawn(
        &program,
        args.split_whitespace(),
        &CommandOptions::default(),
      )
    }
    #[cfg(target_os = "windows")]
    {
      let home_dir =
        home::home_dir().context("Unable to get home directory.")?;

      // TODO: Use `Shell::spawn` instead. `ShellExecuteExW` is still used
      // to be able to launch programs from the App Paths registry
      // (`HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths`), like
      // `chrome` without it being in $PATH.
      state.dispatcher.shell_execute_ex(
        &program,
        &args,
        &home_dir,
        hide_window,
      )
    }
  };

  result.map_err(|err| {
    anyhow::anyhow!(
      "Shell exec failed for '{command}'. Make sure the program exists and is \
      accessible from your shell. Error: {err}",
    )
  })?;

  Ok(())
}

/// Reject empty / quote-junk programs that would ShellExecute into UNC nonsense
/// like `\\ "\"` (Windows "Network Error" dialog), and refuse truncated
/// `C:\\Program` paths that produce the classic Windows "cannot find" popup.
fn validate_shell_program(program: &str, original: &str) -> anyhow::Result<()> {
  let trimmed = program.trim();
  if trimmed.is_empty() {
    anyhow::bail!(
      "Shell exec failed for '{original}': program path is empty."
    );
  }
  // Quotes should have been stripped by parse; leftover quotes mean bad parse.
  if trimmed.contains('"') {
    anyhow::bail!(
      "Shell exec failed for '{original}': program path contains leftover quotes ({trimmed:?})."
    );
  }
  // Lone backslashes / malformed UNC stubs (e.g. `\\` or `\\ `) are never valid.
  if trimmed.chars().all(|c| c == '\\' || c == '/' || c.is_whitespace()) {
    anyhow::bail!(
      "Shell exec failed for '{original}': program path is malformed ({trimmed:?})."
    );
  }
  // Classic unquoted `C:\\Program Files\\...` split: program becomes `C:\\Program`.
  // ShellExecute of that pops "Windows cannot find 'C:\\Program'".
  if is_truncated_program_files_prefix(trimmed) {
    anyhow::bail!(
      "Shell exec failed for '{original}': program path looks like an unquoted        'Program Files' truncation ({trimmed:?}). Quote the full path."
    );
  }
  Ok(())
}

/// `C:\\Program` / `C:/Program` — never a real executable by itself.
fn is_truncated_program_files_prefix(program: &str) -> bool {
  let norm = program.replace('/', "\\");
  let lower = norm.to_ascii_lowercase();
  lower == "c:\\program"
    || lower.ends_with(":\\program")
    || lower == "c:\\program files"
    || lower.ends_with(":\\program files")
    || lower == "c:\\program files (x86)"
    || lower.ends_with(":\\program files (x86)")
}

/// Parses a command string into a program name/path and arguments. This
/// also expands any environment variables found in the command string if
/// they are wrapped in `%` characters. If the command string is a path,
/// a file extension is required.
///
/// This is similar to the `SHEvaluateSystemCommandTemplate` Win32
/// function. It also parses program name/path and arguments, but can't
/// handle `/` as file path delimiters and it errors for certain programs
/// (e.g. `code`).
///
/// Returns a tuple containing the program name/path and arguments.
fn parse_command(
  command: &str,
  // LINT: `state` is only used on Windows.
  #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
  state: &WmState,
) -> anyhow::Result<(String, String)> {
  // Expand environment variables in the command string.
  let expanded_command = {
    #[cfg(target_os = "windows")]
    {
      state.dispatcher.expand_env_strings(command)?
    }
    #[cfg(target_os = "macos")]
    {
      // TODO: Expand env variables on macOS.
      command.to_string()
    }
  };

  parse_expanded_command(&expanded_command).map_err(|err| {
    anyhow::anyhow!("Shell exec failed for '{command}': {err}")
  })
}

/// Parse an already-expanded command string into `(program, args)`.
///
/// Extracted so unit tests can cover quoting without a live `WmState`.
fn parse_expanded_command(
  expanded_command: &str,
) -> anyhow::Result<(String, String)> {
  let command_parts =
    expanded_command.split_whitespace().collect::<Vec<_>>();

  // If the command starts with double quotes, then the program name/path
  // is wrapped in double quotes (e.g. `"C:\path\to\app.exe" --flag`).
  if expanded_command.starts_with('"') {
    // Closing quote is the *second* quote (index 1). Using nth(2) was an
    // off-by-one that required a third quote and turned `"" ""` into
    // program=`" ` (quote+space), which ShellExecute can surface as a
    // Network Error UNC path like `\\ "\"`.
    let (closing_index, _) =
      expanded_command.match_indices('"').nth(1).ok_or_else(|| {
        anyhow::anyhow!("command doesn't have an ending `\"`.")
      })?;

    return Ok((
      expanded_command[1..closing_index].to_string(),
      expanded_command[closing_index + 1..].trim().to_string(),
    ));
  }

  // The first part is the program name if it doesn't contain a slash or
  // backslash.
  if let Some(first_part) = command_parts.first() {
    if !first_part.contains(&['/', '\\'][..]) {
      let args = command_parts[1..].join(" ");
      return Ok(((*first_part).to_string(), args));
    }
  }

  let mut cumulative_path = Vec::new();

  // Lastly, iterate over the command until a valid file path is found.
  for (part_index, &part) in command_parts.iter().enumerate() {
    cumulative_path.push(part);

    if Path::new(&cumulative_path.join(" ")).is_file() {
      return Ok((
        cumulative_path.join(" "),
        command_parts[part_index + 1..].join(" "),
      ));
    }
  }

  // File may not exist yet (or WindowsApps path is virtual). Still recover a
  // spaced path when the command clearly names an exe/bat/cmd/com under a
  // drive root — never return the truncated first token alone.
  if let Some((prog, args)) = recover_spaced_exe_path(&command_parts) {
    if is_truncated_program_files_prefix(&prog) {
      anyhow::bail!(
        "program path is not valid (looks like unquoted Program Files truncation: {prog:?})."
      );
    }
    return Ok((prog, args));
  }

  // Explicit guard: first whitespace token of an unquoted Program Files path.
  if let Some(first) = command_parts.first() {
    if is_truncated_program_files_prefix(first) {
      anyhow::bail!(
        "program path is not valid (unquoted path split at space into {first:?}; quote the full path)."
      );
    }
  }

  anyhow::bail!("program path is not valid.")
}

/// Recover `C:\\Program Files\\app.exe` + args from whitespace-split parts
/// when `Path::is_file` failed (missing file / virtual store path).
fn recover_spaced_exe_path(parts: &[&str]) -> Option<(String, String)> {
  if parts.is_empty() {
    return None;
  }
  let first = parts[0];
  // Must look like a drive-absolute path start (`C:\\...` or `C:/...`).
  let bytes = first.as_bytes();
  if bytes.len() < 3
    || !bytes[0].is_ascii_alphabetic()
    || bytes[1] != b':'
    || (bytes[2] != b'\\' && bytes[2] != b'/')
  {
    return None;
  }

  let mut cumulative = String::new();
  for (idx, part) in parts.iter().enumerate() {
    if idx > 0 {
      cumulative.push(' ');
    }
    cumulative.push_str(part);
    let lower = cumulative.to_ascii_lowercase();
    if lower.ends_with(".exe")
      || lower.ends_with(".bat")
      || lower.ends_with(".cmd")
      || lower.ends_with(".com")
    {
      let args = parts[idx + 1..].join(" ");
      return Some((cumulative, args));
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn quoted_program_with_spaces_uses_closing_quote() {
    let (prog, args) =
      parse_expanded_command(r#""C:\Program Files\app.exe" --flag"#)
        .unwrap();
    assert_eq!(prog, r"C:\Program Files\app.exe");
    assert_eq!(args, "--flag");
    validate_shell_program(&prog, "x").unwrap();
  }

  #[test]
  fn quoted_program_with_quoted_arg() {
    let (prog, args) =
      parse_expanded_command(r#""C:\a b\c.exe" "d e""#).unwrap();
    assert_eq!(prog, r"C:\a b\c.exe");
    // Remainder keeps ShellExecute-style quoting for spaced args.
    assert_eq!(args, r#""d e""#);
    validate_shell_program(&prog, "x").unwrap();
  }

  #[test]
  fn empty_quoted_program_rejected() {
    // Input is two quote chars: empty quoted program.
    let (prog, _args) = parse_expanded_command("\"\"").unwrap();
    assert_eq!(prog, "");
    assert!(validate_shell_program(&prog, "\"\"").is_err());
  }

  #[test]
  fn empty_then_empty_does_not_yield_quote_space_program() {
    // Regression for nth(2) bug: `"" ""` previously became program=`" `.
    let input = "\"\" \"\"";
    let (prog, args) = parse_expanded_command(input).unwrap();
    assert_eq!(prog, "");
    assert_eq!(args, "\"\"");
    assert!(validate_shell_program(&prog, input).is_err());
  }

  #[test]
  fn validate_rejects_quote_junk_and_bare_slashes() {
    assert!(validate_shell_program("\" ", "x").is_err());
    assert!(validate_shell_program(r"\\", "x").is_err());
    assert!(validate_shell_program(r"\ ", "x").is_err());
    assert!(validate_shell_program("zebar", "zebar start").is_ok());
    assert!(validate_shell_program(r"C:\Tools\app.exe", "x").is_ok());
  }

  #[test]
  fn unquoted_simple_program() {
    let (prog, args) =
      parse_expanded_command("zebar start-widget-preset --pack x").unwrap();
    assert_eq!(prog, "zebar");
    assert_eq!(args, "start-widget-preset --pack x");
  }

  #[test]
  fn validate_rejects_truncated_c_program() {
    assert!(validate_shell_program(r"C:\Program", "x").is_err());
    assert!(validate_shell_program(r"C:/Program", "x").is_err());
    assert!(validate_shell_program(r"C:\Program Files", "x").is_err());
    assert!(validate_shell_program(r"C:\Program Files\app.exe", "x").is_ok());
  }

  #[test]
  fn recover_unquoted_program_files_exe_even_if_missing() {
    // is_file() fails for a fake path; recover by .exe suffix across spaces.
    let (prog, args) = parse_expanded_command(
      r"C:\Program Files\glzr.io\GlazeWM\glazewm.exe --foo",
    )
    .unwrap();
    assert_eq!(prog, r"C:\Program Files\glzr.io\GlazeWM\glazewm.exe");
    assert_eq!(args, "--foo");
    validate_shell_program(&prog, "x").unwrap();
  }

  #[test]
  fn bare_c_program_token_errors_clearly() {
    let err = parse_expanded_command(r"C:\Program Files\missing\nope.bin")
      .unwrap_err()
      .to_string();
    assert!(
      err.contains("Program Files") || err.contains("not valid"),
      "unexpected err: {err}"
    );
  }
}
