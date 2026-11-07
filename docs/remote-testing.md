# Remote Windows test runner

The runner builds Rust unit, integration, and documentation test executables
locally, then runs them on a configured Windows host. The remote host does not
need the GlazeWM source tree or Rust toolchain. Locally built non-system DLL
dependencies are uploaded beside the test executables.

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

Tests are launched automatically in the active Windows console session using
the logged-on user's interactive token. The runner finds the active console
session and logged-on user at runtime, then verifies that the worker started in
that session and can see a monitor. If no connected console session with a
logged-on user and visible monitor can be found, it reports that session error
instead of running GUI-dependent tests in the SSH service session. No session
ID flag is needed.

Rustdoc compiles doctests with `--no-run` and persists their executables in the
local run directory. The runner uploads and executes runnable examples on the
remote host with the rest of the suite. Compile-only `no_run` examples and
compile-fail examples are checked during the local Rustdoc build and are not
launched as programs on the VM.

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

`--filter` accepts the same case-sensitive name substring used by Rust's test
harness. Before connecting, the runner lists tests in every built unit or
integration executable, excludes ignored tests, and uploads only executables
with at least one runnable match. A misspelled or ignored-only filter fails
with exit code 2 before transfer; an executable with no match is reported as
`skipped (no match)`. The summary reports executable passes/failures/skips
separately from test case passes/failures/ignored counts. Combine the filter
with `--package` to avoid building unrelated targets. Use `--test-threads 1`
to override the local config for a run. Filtered runs omit doctest binaries;
run a package without `--filter` to include its documentation examples.

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

The runner's focused accounting and timeout regression checks can be run with:

```powershell
python -m unittest scripts.test_remote_tests
```

## Progress and retained logs

Progress is printed as each build message, upload, remote test line, and
completion status arrives. The runner also prints a heartbeat while a build,
transfer, or test is quiet. Each run is written under the Git-ignored local
directory `Temp/remote-tests/<run-id>/`. These logs are local artifacts and
are not committed or automatically backed up; copy them elsewhere if they
need to be retained beyond local cleanup. The remote run directory is also
retained for inspection. Each local run contains:

- `run.log` — readable combined progress and results;
- `build.jsonl` — Cargo's raw machine-readable build stream;
- `doctest-build.log` — doctest compilation and persistence progress;
- `preflight.log` — listed test names for filtered runs;
- `transfer.log` — per-file upload starts, heartbeats, sizes, rates, and errors;
- `tests/*.log` — streamed output for each remote test executable;
- `summary.json` — scope, run times, executable pass/fail/skip counts, parsed
  test case counts, and remote directory.

The remote run directory also contains the uploaded executables, runtime DLLs,
and PowerShell tee logs. Local build, transfer, and remote execution deadlines
are checked while output is arriving as well as while the process is quiet.
On timeout the runner terminates and reaps the local process tree, asks the
remote controller to terminate the uniquely named test process and helper
tree, and returns a failing status. A remote test pass requires both a
successful SSH/controller exit and exactly one valid worker exit marker.
