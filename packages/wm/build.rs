use tauri_winres::VersionInfo;

fn spotco_build_id() -> String {
  if let Ok(v) = std::env::var("SPOTCO_BUILD_ID") {
    if !v.is_empty() {
      return v;
    }
  }

  // Local time (this machine is America/New_York). Prefer PowerShell for a
  // stable yyyyMMdd-HHmmss stamp without adding build-deps.
  let ps = std::process::Command::new("powershell")
    .args([
      "-NoProfile",
      "-Command",
      "Get-Date -Format 'yyyyMMdd-HHmmss'",
    ])
    .output();

  if let Ok(out) = ps {
    if out.status.success() {
      let stamp = String::from_utf8_lossy(&out.stdout).trim().to_string();
      if !stamp.is_empty() {
        return format!("spotcobuild-{stamp}");
      }
    }
  }

  format!(
    "spotcobuild-unknown-{}",
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map(|d| d.as_secs())
      .unwrap_or(0)
  )
}

fn main() {
  // Re-run when build.bat sets a fresh trigger so each release build gets
  // a new spotcobuild stamp even if sources are unchanged.
  println!("cargo:rerun-if-env-changed=SPOTCO_BUILD_TRIGGER");
  println!("cargo:rerun-if-env-changed=SPOTCO_BUILD_ID");
  println!("cargo:rerun-if-env-changed=VERSION_NUMBER");

  let build_id = spotco_build_id();
  println!("cargo:rustc-env=SPOTCO_BUILD_ID={build_id}");

  let mut res = tauri_winres::WindowsResource::new();

  // When the `ui_access` feature is enabled, the `uiAccess` attribute is
  // set to `true`. UIAccess is disabled by default because it requires the
  // application to be signed and installed in a secure location.
  let ui_access = {
    #[cfg(feature = "ui_access")]
    {
      "true"
    }
    #[cfg(not(feature = "ui_access"))]
    {
      "false"
    }
  };

  // Conditionally enable UIAccess, which grants privilege to set the
  // foreground window and to set the position of elevated windows.
  //
  // Ref: https://learn.microsoft.com/en-us/previous-versions/windows/it-pro/windows-10/security/threat-protection/security-policy-settings/user-account-control-only-elevate-uiaccess-applications-that-are-installed-in-secure-locations
  //
  // Additionally, declare support for per-monitor DPI awareness.
  let manifest_str = format!(
    r#"
<assembly
  xmlns="urn:schemas-microsoft-com:asm.v1"
  manifestVersion="1.0"
  xmlns:asmv3="urn:schemas-microsoft-com:asm.v3"
>
  <asmv3:trustInfo>
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="{ui_access}" />
      </requestedPrivileges>
    </security>
  </asmv3:trustInfo>

  <asmv3:application>
    <windowsSettings
      xmlns:ws2005="http://schemas.microsoft.com/SMI/2005/WindowsSettings"
      xmlns:ws2016="http://schemas.microsoft.com/SMI/2016/WindowsSettings"
    >
      <ws2005:dpiAware>true</ws2005:dpiAware>
      <ws2016:dpiAwareness>PerMonitorV2</ws2016:dpiAwareness>
    </windowsSettings>
  </asmv3:application>
</assembly>
"#
  );

  res.set_manifest(&manifest_str);
  res.set_icon("../../resources/assets/icon.ico");

  // Set language to English (US).
  res.set_language(0x0409);

  res.set("OriginalFilename", "glazewm.exe");
  res.set("ProductName", "GlazeWM");
  res.set("FileDescription", "GlazeWM");

  let version_parts = env!("VERSION_NUMBER")
    .split('.')
    .take(3)
    .map(|part| part.parse().unwrap_or(0))
    .collect::<Vec<u16>>();

  let [major, minor, patch] =
    <[u16; 3]>::try_from(version_parts).unwrap_or([0, 0, 0]);

  let version_str = format!("{major}.{minor}.{patch}.0");
  res.set("FileVersion", &version_str);
  res.set("ProductVersion", &version_str);

  let version_u64 = (u64::from(major) << 48)
    | (u64::from(minor) << 32)
    | (u64::from(patch) << 16);

  res.set_version_info(VersionInfo::FILEVERSION, version_u64);
  res.set_version_info(VersionInfo::PRODUCTVERSION, version_u64);

  res.compile().unwrap();
}
