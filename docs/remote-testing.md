# Remote Windows test runner

The runner builds Rust test executables on the local checkout and runs those
executables on a configured Windows host. The remote host does not need the
GlazeWM source tree or Rust toolchain. Locally built non-system DLL dependencies
are uploaded beside the test executables.

## Setup

Use Python 3.10 or later, the Rust toolchain used by the project, and OpenSSH.
For password-authenticated hosts, the optional `network_computers` provider
uses the shared network-computers Python package and reads its password file.
For normal OpenSSH use, configure an SSH agent or private-key file.

Copy `config_local.example.json` to the ignored root `config_local.json`, then
fill in the remote connection and destination settings. That file is ignored by
Git and is the only place for machine names, usernames, local credential paths,
private-key paths, and remote paths. Do not put a password or private-key
contents in the config; point to the existing local credential file or use the
SSH agent.

The `connection` block supports these providers:

- `openssh`: set `host`, optional `user`, `port`, and optional
  `identity_file`. If `identity_file` is empty, OpenSSH uses the configured
  agent and user SSH config.
- `network_computers`: set `nickname`, `network_computers_root`, and
  `password_file`. The nickname is resolved from the shared host inventory.
  The root and password-file path are local machine settings and stay in the
  ignored config.

`remote.directory` is a writable directory on the Windows host. Each run gets
a timestamped subdirectory, so a previous or still-locked executable cannot
block a new upload. The runner keeps those remote directories for inspection.

## Commands

Run the full Cargo workspace test suite on the remote host:

```powershell
python scripts/remote_tests.py
```

Run all tests in one package:

```powershell
python scripts/remote_tests.py --package wm-platform
```

Run a specific test by name substring:

```powershell
python scripts/remote_tests.py --package wm-platform --filter hung_hwnd_rejoins_after_suspend_past_early_recovery_window
```

`--filter` accepts the same name substring used by Rust's test harness. Combine
it with `--package` to avoid starting unrelated test executables. Use
`--test-threads 1` to override the local config for a run.

The script normally builds the checkout that contains it. To reuse the runner
from one worktree while building another, pass `--project-root <checkout>`.
If that checkout does not contain its own ignored config, pass `--config
<local-config-file>` as well.

Useful preparation commands:

```powershell
python scripts/remote_tests.py --dry-run
python scripts/remote_tests.py --package wm-platform --build-only
```

`--dry-run` validates the local config without building or connecting.
`--build-only` compiles the selected test binaries and retains the build log
without contacting the Windows host.

## Progress and retained logs

Progress is printed as each build message, upload, remote test line, and
completion status arrives. The runner also prints a heartbeat while a build,
transfer, or test is quiet. Each run is preserved under
`Temp/remote-tests/<run-id>/`:

- `run.log` — readable combined progress and results;
- `build.jsonl` — Cargo's raw machine-readable build stream;
- `transfer.log` — per-file upload starts, heartbeats, sizes, rates, and errors;
- `tests/*.log` — streamed output for each remote test executable;
- `summary.json` — scope, run times, remote directory, and pass/fail totals.

The remote run directory also contains the uploaded executables, runtime DLLs,
and PowerShell tee logs. The test timeout is bounded; on timeout the runner
terminates the uniquely named remote test process and its helper-process tree.
