use std::{
  env, ffi::OsStr, iter, mem::size_of, os::windows::ffi::OsStrExt,
};

use windows::{
  core::PCWSTR,
  Win32::{
    Foundation::{CloseHandle, HANDLE},
    Security::{
      GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
    UI::{
      Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW},
      WindowsAndMessaging::SW_SHOWNORMAL,
    },
  },
};

const ERROR_CANCELLED_HRESULT: i32 = 0x8007_04C7_u32.cast_signed();

/// Returns whether the current process has a full elevated token.
pub(crate) fn is_process_elevated() -> crate::Result<bool> {
  let mut token = HANDLE::default();
  unsafe {
    OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token)?;
  }

  let mut elevation = TOKEN_ELEVATION::default();
  let mut returned_length = 0;
  let result = unsafe {
    GetTokenInformation(
      token,
      TokenElevation,
      Some((&raw mut elevation).cast()),
      u32::try_from(size_of::<TOKEN_ELEVATION>())?,
      &raw mut returned_length,
    )
  };
  unsafe {
    let _ = CloseHandle(token);
  }
  result?;

  Ok(elevation.TokenIsElevated != 0)
}

/// Starts the current executable again with the Windows `runas` verb.
///
/// Returns `true` when the elevated process was launched, and `false` when
/// the user dismissed the UAC prompt.
pub(crate) fn relaunch_current_process_as_admin(
  args: &[String],
) -> crate::Result<bool> {
  let executable = env::current_exe()?;
  let executable_wide = wide_null(executable.as_os_str());
  let verb_wide = wide_null(OsStr::new("runas"));
  let parameters_wide = encode_windows_arguments(args);

  let mut execute_info = SHELLEXECUTEINFOW {
    cbSize: u32::try_from(size_of::<SHELLEXECUTEINFOW>())?,
    fMask: SEE_MASK_NOASYNC,
    lpVerb: PCWSTR(verb_wide.as_ptr()),
    lpFile: PCWSTR(executable_wide.as_ptr()),
    lpParameters: PCWSTR(parameters_wide.as_ptr()),
    nShow: SW_SHOWNORMAL.0,
    ..Default::default()
  };

  match unsafe { ShellExecuteExW(&raw mut execute_info) } {
    Ok(()) => Ok(true),
    Err(error) if error.code().0 == ERROR_CANCELLED_HRESULT => Ok(false),
    Err(error) => Err(error.into()),
  }
}

fn wide_null(value: &OsStr) -> Vec<u16> {
  value.encode_wide().chain(iter::once(0)).collect()
}

/// Quotes arguments using the Windows command-line backslash/quote rules.
fn encode_windows_arguments(args: &[String]) -> Vec<u16> {
  let mut encoded = Vec::new();
  let backslash = u16::from(b'\\');
  let quote = u16::from(b'"');

  for (index, arg) in args.iter().enumerate() {
    if index > 0 {
      encoded.push(u16::from(b' '));
    }

    encoded.push(quote);
    let mut backslashes = 0;
    for unit in arg.encode_utf16() {
      if unit == backslash {
        backslashes += 1;
      } else if unit == quote {
        encoded.extend(iter::repeat_n(backslash, backslashes * 2 + 1));
        encoded.push(unit);
        backslashes = 0;
      } else {
        encoded.extend(iter::repeat_n(backslash, backslashes));
        encoded.push(unit);
        backslashes = 0;
      }
    }

    encoded.extend(iter::repeat_n(backslash, backslashes * 2));
    encoded.push(quote);
  }

  encoded.push(0);
  encoded
}
