#!/usr/bin/env python3
"""Build Rust test binaries locally, then upload and run them on a Windows host.

The machine-specific connection settings live in the ignored root
``config_local.json``. All run output is retained under ``Temp/remote-tests``.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import queue
import re
import shlex
import shutil
import struct
import subprocess
import sys
import threading
import time
import uuid
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable


PROJECT_ROOT = Path(__file__).resolve().parents[1]
CONFIG_ROOT = PROJECT_ROOT
RUN_ROOT = PROJECT_ROOT / "Temp" / "remote-tests"
EXIT_MARKER = "REMOTE_TEST_EXIT_CODE="


class RunnerError(RuntimeError):
    """A configuration, build, transfer, or remote execution error."""


@dataclass(frozen=True)
class TestArtifact:
    package: str
    target: str
    executable: Path


@dataclass(frozen=True)
class RemoteTestOutcome:
    exit_code: int
    test_summary: dict[str, int] | None


TEST_RESULT_RE = re.compile(
    r"^test result: (?:ok|FAILED)\.\s+(\d+) passed; (\d+) failed; "
    r"(\d+) ignored;"
)


def terminate_process_tree(
    process: subprocess.Popen[Any], *, wait_timeout: float = 10
) -> None:
    """Terminate and reap a timed-out command and its descendants."""
    if os.name == "nt":
        try:
            subprocess.run(
                ["taskkill.exe", "/PID", str(process.pid), "/T", "/F"],
                capture_output=True,
                check=False,
                timeout=wait_timeout,
            )
        except (OSError, subprocess.TimeoutExpired):
            pass
    elif process.poll() is None:
        process.terminate()

    try:
        process.wait(timeout=wait_timeout)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=wait_timeout)


def close_process_output(process: subprocess.Popen[Any]) -> None:
    if process.stdout is not None:
        process.stdout.close()


def parse_test_summary(line: str) -> dict[str, int] | None:
    match = TEST_RESULT_RE.match(line.strip())
    if match is None:
        return None
    return {
        "passed": int(match.group(1)),
        "failed": int(match.group(2)),
        "ignored": int(match.group(3)),
    }


def parse_listed_test_names(output: str) -> list[str]:
    """Parse names emitted by Rust's test harness `--list` option."""
    names = []
    for line in output.splitlines():
        name, separator, kind = line.rpartition(": ")
        if separator and kind == "test" and name:
            names.append(name)
    return names


def matching_test_names(
    names: list[str], ignored_names: set[str], test_filter: str
) -> list[str]:
    return [
        name
        for name in names
        if test_filter in name and name not in ignored_names
    ]


def select_filtered_artifacts(
    artifacts: list[TestArtifact],
    test_filter: str,
    *,
    timeout: int,
    logger: RunLog,
    run_dir: Path,
) -> tuple[list[TestArtifact], int, int]:
    """Preflight Rust test names and select only executables with a match."""
    selected: list[TestArtifact] = []
    skipped = 0
    matching_tests = 0
    preflight_log = run_dir / "preflight.log"
    with preflight_log.open("w", encoding="utf-8", newline="") as log:
        for artifact in artifacts:
            result = run_local_capture(
                [str(artifact.executable), "--list"], timeout=timeout
            )
            ignored_result = run_local_capture(
                [str(artifact.executable), "--ignored", "--list"],
                timeout=timeout,
            )
            log.write(
                f"## {artifact.package}/{artifact.target} "
                f"list_exit={result.returncode} ignored_list_exit="
                f"{ignored_result.returncode}\n{result.stdout}\n"
                f"--ignored --list\n{ignored_result.stdout}\n"
            )
            if result.returncode != 0 or ignored_result.returncode != 0:
                raise RunnerError(
                    f"Could not list tests in {artifact.package}/{artifact.target}; "
                    f"exit(s) {result.returncode}/{ignored_result.returncode}. "
                    f"See {preflight_log}."
                )
            names = parse_listed_test_names(result.stdout)
            ignored_names = set(
                parse_listed_test_names(ignored_result.stdout)
            )
            matches = matching_test_names(names, ignored_names, test_filter)
            matching_tests += len(matches)
            if matches:
                selected.append(artifact)
                logger.write(
                    f"[preflight] {artifact.package}/{artifact.target}: "
                    f"{len(names)} named tests, {len(ignored_names)} ignored, "
                    f"{len(matches)} runnable filter match(es); will execute"
                )
            else:
                skipped += 1
                logger.write(
                    f"[preflight] {artifact.package}/{artifact.target}: "
                    f"{len(names)} named tests, {len(ignored_names)} ignored, "
                    "zero runnable match; skipped"
                )
    logger.write(
        f"[preflight] filter matched {matching_tests} runnable test names across "
        f"{len(selected)} executable(s); {skipped} executable(s) skipped"
    )
    return selected, skipped, matching_tests


def resolve_remote_exit_code(ssh_exit: int, markers: list[str]) -> int:
    """Require a successful controller and exactly one valid worker marker."""
    if ssh_exit != 0:
        return ssh_exit
    if len(markers) != 1:
        return 125
    marker = markers[0].strip()
    if not marker.isdecimal():
        return 125
    code = int(marker)
    return code if code <= 255 else 125


def run_local_capture(
    command: list[str], *, timeout: int, cwd: Path | None = None
) -> subprocess.CompletedProcess[str]:
    """Capture a bounded local command, killing and reaping it on timeout."""
    process = subprocess.Popen(
        command,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    try:
        stdout, _ = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired as error:
        terminate_process_tree(process)
        stdout, _ = process.communicate()
        raise RunnerError(
            f"Command timed out after {timeout}s: {' '.join(command)}\n{stdout}"
        ) from error
    return subprocess.CompletedProcess(command, process.returncode, stdout, "")


class RunLog:
    """Writes progress to the console and retained UTF-8 run logs."""

    def __init__(self, run_dir: Path) -> None:
        self.run_dir = run_dir
        self.run_dir.mkdir(parents=True, exist_ok=True)
        self._lock = threading.Lock()
        self._run_log = (run_dir / "run.log").open(
            "a", encoding="utf-8", newline=""
        )

    def write(
        self,
        message: str,
        *,
        mirror: Path | None = None,
        console: bool = True,
    ) -> None:
        line = message.rstrip("\r\n")
        with self._lock:
            if console:
                print(line, flush=True)
            self._run_log.write(line + "\n")
            self._run_log.flush()
            if mirror is not None:
                mirror.parent.mkdir(parents=True, exist_ok=True)
                with mirror.open("a", encoding="utf-8", newline="") as handle:
                    handle.write(line + "\n")

    def close(self) -> None:
        self._run_log.close()


def local_path(
    value: str | Path, *, base: Path | None = None
) -> Path:
    path = Path(value).expanduser()
    if not path.is_absolute():
        path = (base or CONFIG_ROOT) / path
    return path.resolve()


def load_config(config_path: Path) -> dict[str, Any]:
    if not config_path.is_file():
        example = PROJECT_ROOT / "config_local.example.json"
        raise RunnerError(
            f"Local config not found: {config_path}\n"
            f"Copy {example.name} to config_local.json and fill in the "
            "remote connection settings."
        )
    try:
        config = json.loads(config_path.read_text(encoding="utf-8-sig"))
    except (OSError, json.JSONDecodeError) as error:
        raise RunnerError(f"Could not read {config_path}: {error}") from error

    if not isinstance(config, dict):
        raise RunnerError("The local config must contain a JSON object.")
    connection = config.get("connection")
    remote = config.get("remote")
    timeouts = config.get("timeouts", {})
    if not isinstance(connection, dict) or not isinstance(remote, dict):
        raise RunnerError("Config requires 'connection' and 'remote' objects.")
    if not isinstance(timeouts, dict):
        raise RunnerError("Config 'timeouts' must be a JSON object.")
    provider = connection.get("provider", "openssh")
    if provider not in {"openssh", "network_computers"}:
        raise RunnerError(
            "connection.provider must be 'openssh' or 'network_computers'."
        )
    if not str(remote.get("directory", "")).strip():
        raise RunnerError("remote.directory must be a Windows directory path.")
    if provider == "openssh" and not str(connection.get("host", "")).strip():
        raise RunnerError("connection.host is required for the openssh provider.")
    if provider == "network_computers" and not str(
        connection.get("nickname", "")
    ).strip():
        raise RunnerError(
            "connection.nickname is required for the network_computers provider."
        )
    return config


def powershell_quote(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def remote_path_join(directory: str, name: str) -> str:
    return directory.rstrip("/\\") + "/" + name


class RemoteConnection:
    """SSH/SCP access using either OpenSSH keys or the optional shared auth SDK."""

    def __init__(self, config: dict[str, Any]) -> None:
        connection = config["connection"]
        self.provider = str(connection.get("provider", "openssh"))
        self.client: Any = None
        self.target: Any = None
        self.target_text = ""
        self.ssh_executable = ""
        self.scp_executable = ""
        self.option_args: list[str] = []
        self.env = os.environ.copy()

        if self.provider == "network_computers":
            self._configure_network_computers(connection, config)
        else:
            self._configure_openssh(connection)

    def _configure_network_computers(
        self, connection: dict[str, Any], config: dict[str, Any]
    ) -> None:
        root_value = connection.get("network_computers_root")
        if not root_value:
            raise RunnerError(
                "connection.network_computers_root is required for the "
                "network_computers provider."
            )
        root = local_path(str(root_value))
        python_dir = root / "python"
        if not python_dir.is_dir():
            raise RunnerError(
                f"network_computers Python package was not found under {python_dir}."
            )

        password_file = connection.get("password_file")
        if password_file:
            self.env["NETWORK_SSH_PASSWORD_FILE"] = str(
                local_path(str(password_file))
            )
            os.environ["NETWORK_SSH_PASSWORD_FILE"] = self.env[
                "NETWORK_SSH_PASSWORD_FILE"
            ]

        sys.path.insert(0, str(python_dir))
        try:
            from network_computers.auth import resolve_auth
            from network_computers.hosts import load_hosts
            from network_computers.ssh import SshClient, SshTarget
        except ImportError as error:
            raise RunnerError(
                f"Could not import network_computers from {python_dir}: {error}"
            ) from error

        try:
            auth = resolve_auth(root=root, required=True)
            client = SshClient(
                auth,
                connect_timeout=int(
                    config.get("timeouts", {}).get("connect_seconds", 30)
                ),
            )
            host = load_hosts(root=root).get(str(connection["nickname"]))
        except Exception as error:
            raise RunnerError(f"Could not resolve remote credentials/host: {error}") from error

        if host is None:
            raise RunnerError(
                f"Host nickname {connection['nickname']!r} was not found in the "
                "configured network-computers inventory."
            )
        self.client = client
        self.target = SshTarget(
            host=host.address or host.host,
            user=host.user,
            nickname=host.nickname,
            label=host.label,
            remote_os=host.hardware.os,
        )
        self.target_text = self.target.target
        self.ssh_executable = client.ssh_executable
        self.scp_executable = ""
        self.option_args = client._option_args()
        self.env = client.env()
        if password_file:
            self.env["NETWORK_SSH_PASSWORD_FILE"] = str(
                local_path(str(password_file))
            )

    def _configure_openssh(self, connection: dict[str, Any]) -> None:
        ssh_name = str(connection.get("ssh_executable", "ssh"))
        scp_name = str(connection.get("scp_executable", "scp"))
        self.ssh_executable = shutil.which(ssh_name) or ""
        self.scp_executable = shutil.which(scp_name) or ""
        if not self.ssh_executable or not self.scp_executable:
            raise RunnerError("OpenSSH 'ssh' and 'scp' must be available on PATH.")

        host = str(connection["host"]).strip()
        user = str(connection.get("user", "")).strip()
        self.target_text = host if "@" in host or not user else f"{user}@{host}"
        self.option_args = [
            "-o",
            f"ConnectTimeout={int(connection.get('connect_timeout_seconds', 30))}",
            "-p",
            str(int(connection.get("port", 22))),
        ]
        identity_file = connection.get("identity_file")
        if identity_file:
            self.option_args.extend(["-i", str(local_path(str(identity_file)))])
        for option in connection.get("ssh_options", []):
            self.option_args.extend(["-o", str(option)])

    def ssh_argv(self, command: str) -> list[str]:
        return [
            self.ssh_executable,
            "-T",
            *self.option_args,
            self.target_text,
            command,
        ]

    def scp_argv(self, local: Path, remote: str) -> list[str]:
        if self.provider == "network_computers":
            raise RunnerError("Use the configured SSH client for this transfer.")
        port = "22"
        copy_options: list[str] = []
        index = 0
        while index < len(self.option_args):
            option = self.option_args[index]
            value = self.option_args[index + 1] if index + 1 < len(self.option_args) else ""
            if option == "-p" and value:
                port = value
            elif option in {"-i", "-o"} and value:
                copy_options.extend([option, value])
            index += 2
        # OpenSSH scp uses uppercase -P for the same configured port.
        args = [self.scp_executable, "-v", "-P", port, *copy_options]
        args.extend([str(local), f"{self.target_text}:{remote}"])
        return args

    def upload(
        self,
        local: Path,
        remote: str,
        *,
        timeout: int,
        progress: Callable[[str], None],
    ) -> None:
        if self.provider == "network_computers":
            outcome: dict[str, BaseException | None] = {"error": None}
            finished = threading.Event()

            def transfer() -> None:
                try:
                    self.client.scp_to(
                        self.target,
                        local,
                        remote,
                        timeout=timeout,
                        check=True,
                        compress=False,
                    )
                except BaseException as error:
                    outcome["error"] = error
                finally:
                    finished.set()

            threading.Thread(target=transfer, daemon=True).start()
            started = time.monotonic()
            while True:
                elapsed = time.monotonic() - started
                remaining = timeout - elapsed
                if remaining <= 0:
                    raise RunnerError(
                        f"SCP to {remote} timed out after {timeout}s."
                    )
                if finished.wait(min(5, remaining)):
                    break
                progress(f"still transferring ({time.monotonic() - started:.0f}s)")
            if outcome["error"] is not None:
                raise RunnerError(f"SCP failed for {remote}: {outcome['error']}")
            return

        process = subprocess.Popen(
            self.scp_argv(local, remote),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            env=self.env,
            text=True,
            encoding="utf-8",
            errors="replace",
            bufsize=1,
        )
        assert process.stdout is not None
        lines: queue.Queue[str | None] = queue.Queue()

        def read_output() -> None:
            for line in process.stdout:
                lines.put(line.rstrip())
            lines.put(None)

        reader = threading.Thread(target=read_output, daemon=True)
        reader.start()
        started = time.monotonic()
        deadline = started + timeout
        last_progress = started
        ended = False
        while process.poll() is None or not ended:
            now = time.monotonic()
            if now >= deadline:
                terminate_process_tree(process)
                reader.join(timeout=5)
                close_process_output(process)
                raise RunnerError(f"SCP timed out after {timeout}s for {remote}.")
            try:
                line = lines.get(timeout=min(1, deadline - now))
                if line is None:
                    ended = True
                else:
                    progress(line)
                    last_progress = time.monotonic()
            except queue.Empty:
                if process.poll() is not None:
                    ended = True
                if time.monotonic() - last_progress >= 5:
                    progress(f"still transferring ({time.monotonic() - started:.0f}s)")
                    last_progress = time.monotonic()
        reader.join(timeout=5)
        close_process_output(process)
        if process.returncode != 0:
            raise RunnerError(f"SCP exited with code {process.returncode} for {remote}.")

    def run_capture(self, command: str, *, timeout: int) -> subprocess.CompletedProcess[str]:
        process = subprocess.Popen(
            self.ssh_argv(command),
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired as error:
            terminate_process_tree(process)
            stdout, stderr = process.communicate()
            raise RunnerError(
                f"SSH command timed out after {timeout}s: {command}\n"
                f"{stdout}\n{stderr}"
            ) from error
        return subprocess.CompletedProcess(
            process.args, process.returncode, stdout, stderr
        )


def cargo_command(package: str | None) -> list[str]:
    command = ["cargo", "test", "--no-run", "--message-format=json"]
    if package:
        command[2:2] = ["--package", package]
    else:
        command[2:2] = ["--workspace"]
    return command


def package_name(package_id: str, manifest_path: str) -> str:
    package_id_tail = package_id.rsplit("#", 1)[-1]
    if "@" in package_id_tail:
        return package_id_tail.rsplit("@", 1)[0]
    manifest = Path(manifest_path)
    if manifest.name.casefold() == "cargo.toml":
        return manifest.parent.name
    return package_id_tail


def compile_test_binaries(
    package: str | None,
    logger: RunLog,
    run_dir: Path,
    *,
    timeout: int,
) -> list[TestArtifact]:
    command = cargo_command(package)
    logger.write("[build] " + " ".join(command))
    raw_log = run_dir / "build.jsonl"
    process = subprocess.Popen(
        command,
        cwd=PROJECT_ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
        bufsize=1,
    )
    assert process.stdout is not None
    lines: queue.Queue[str | None] = queue.Queue()

    def read_output() -> None:
        for line in process.stdout:
            lines.put(line)
        lines.put(None)

    reader = threading.Thread(target=read_output, daemon=True)
    reader.start()
    artifacts: list[TestArtifact] = []
    seen: set[str] = set()
    started = time.monotonic()
    deadline = started + timeout
    last_output = started
    reader_done = False
    with raw_log.open("w", encoding="utf-8", newline="") as handle:
        while process.poll() is None or not reader_done:
            now = time.monotonic()
            if now >= deadline:
                terminate_process_tree(process)
                reader.join(timeout=5)
                close_process_output(process)
                raise RunnerError(f"Local Cargo build timed out after {timeout}s.")
            try:
                raw_line = lines.get(timeout=min(1, deadline - now))
            except queue.Empty:
                if process.poll() is not None:
                    reader_done = True
                if time.monotonic() - last_output >= 5:
                    logger.write(
                        f"[build] still compiling ({time.monotonic() - started:.0f}s)"
                    )
                    last_output = time.monotonic()
                continue

            if raw_line is None:
                reader_done = True
                continue
            handle.write(raw_line)
            handle.flush()
            last_output = time.monotonic()
            try:
                message = json.loads(raw_line)
            except json.JSONDecodeError:
                logger.write("[build] " + raw_line.rstrip())
                continue

            reason = message.get("reason")
            if reason == "compiler-message":
                rendered = message.get("message", {}).get("rendered")
                if rendered:
                    for rendered_line in rendered.rstrip().splitlines():
                        logger.write("[build] " + rendered_line)
            elif reason == "compiler-artifact":
                target = message.get("target", {})
                executable = message.get("executable")
                if target.get("test") is not True or not executable:
                    continue
                path = Path(executable)
                key = str(path.resolve()).casefold()
                if key in seen or not path.is_file():
                    continue
                seen.add(key)
                artifact = TestArtifact(
                    package=package_name(
                        str(message.get("package_id", "unknown")),
                        str(message.get("manifest_path", "")),
                    ),
                    target=str(target.get("name", "test")),
                    executable=path,
                )
                artifacts.append(artifact)
                logger.write(
                    f"[build] {artifact.package}/{artifact.target}: "
                    f"{artifact.executable.name}"
                )

    reader.join(timeout=5)
    close_process_output(process)
    code = process.wait()
    logger.write(f"[build] Cargo exited {code} after {time.monotonic() - started:.1f}s")
    if code != 0:
        raise RunnerError(f"Cargo test-binary build failed with exit code {code}.")
    if not artifacts:
        raise RunnerError("Cargo produced no test executables for the selected scope.")
    return artifacts


def compile_doctest_binaries(
    package: str | None,
    logger: RunLog,
    run_dir: Path,
    *,
    timeout: int,
) -> list[TestArtifact]:
    """Persist locally built doctest executables so they can run on the host."""
    persist_dir = run_dir / "doctest-binaries"
    persist_dir.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    encoded_flags = env.get("CARGO_ENCODED_RUSTDOCFLAGS")
    if encoded_flags:
        rustdoc_flags = encoded_flags.split("\x1f")
    else:
        rustdoc_flags = shlex.split(
            env.get("RUSTDOCFLAGS", ""), posix=os.name != "nt"
        )
    rustdoc_flags.extend(
        [
            "-Z",
            "unstable-options",
            "--persist-doctests",
            str(persist_dir.resolve()),
            "--no-run",
        ]
    )
    env["CARGO_ENCODED_RUSTDOCFLAGS"] = "\x1f".join(rustdoc_flags)

    command = ["cargo", "test"]
    if package:
        command.extend(["--package", package])
    else:
        command.append("--workspace")
    command.append("--doc")
    logger.write("[doctest-build] " + " ".join(command))
    code = stream_local_process(
        command,
        env=env,
        timeout=timeout,
        label="doctest-build",
        logger=logger,
        mirror=run_dir / "doctest-build.log",
        cwd=PROJECT_ROOT,
    )
    if code != 0:
        raise RunnerError(f"Cargo doctest build failed with exit code {code}.")

    executables = sorted(
        persist_dir.rglob("*.exe"), key=lambda path: path.as_posix().casefold()
    )
    artifacts: list[TestArtifact] = []
    for executable in executables:
        if is_compile_only_doctest(executable):
            logger.write(
                f"[doctest-build] skipping compile-only example: "
                f"{executable.parent.name}"
            )
            continue
        artifacts.append(
            TestArtifact(
                package="doctest",
                target=executable.parent.relative_to(persist_dir).as_posix(),
                executable=executable,
            )
        )
    logger.write(
        f"[doctest-build] Cargo exited 0; persisted "
        f"{len(artifacts)} doctest executables"
    )
    return artifacts


def is_compile_only_doctest(executable: Path) -> bool:
    """Identify `no_run` doctests whose persisted snippet must not be executed."""
    match = re.fullmatch(r"(.+)_([0-9]+)_([0-9]+)", executable.parent.name)
    if not match:
        return False

    source_id, line_number = match.group(1), int(match.group(2))
    packages_dir = PROJECT_ROOT / "packages"
    if not packages_dir.is_dir():
        return False

    for source in packages_dir.rglob("*.rs"):
        normalized = re.sub(
            r"[^A-Za-z0-9]+", "_", source.relative_to(PROJECT_ROOT).as_posix()
        ).strip("_")
        if normalized != source_id:
            continue
        lines = source.read_text(encoding="utf-8").splitlines()
        if 0 < line_number <= len(lines):
            return "no_run" in lines[line_number - 1]
    return False


def clean_remote_output_line(line: str) -> str | None:
    """Drop PowerShell's serialized progress chatter while preserving errors."""
    if line.strip() == "#< CLIXML":
        return None
    if not line.lstrip().startswith("<Objs "):
        return line
    try:
        root = ET.fromstring(line)
    except ET.ParseError:
        return line
    records = [
        element
        for element in root.iter()
        if element.tag.rsplit("}", 1)[-1] == "Obj"
    ]
    if records and all(record.get("S") == "progress" for record in records):
        return None
    return line


def pe_imports(path: Path) -> list[str]:
    """Read imported DLL names from a PE32/PE32+ image using the stdlib only."""

    data = path.read_bytes()
    if len(data) < 0x40 or data[:2] != b"MZ":
        raise RunnerError(f"Expected a Windows PE executable, got {path}.")
    pe_offset = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe_offset : pe_offset + 4] != b"PE\0\0":
        raise RunnerError(f"Invalid PE signature in {path}.")

    coff = pe_offset + 4
    section_count = struct.unpack_from("<H", data, coff + 2)[0]
    optional_size = struct.unpack_from("<H", data, coff + 16)[0]
    optional = coff + 20
    magic = struct.unpack_from("<H", data, optional)[0]
    if magic == 0x20B:
        directory_offset = optional + 112
    elif magic == 0x10B:
        directory_offset = optional + 96
    else:
        raise RunnerError(f"Unknown PE optional-header format in {path}.")

    import_rva, import_size = struct.unpack_from("<II", data, directory_offset + 8)
    if not import_rva or not import_size:
        return []

    section_start = optional + optional_size
    sections: list[tuple[int, int, int, int]] = []
    for index in range(section_count):
        offset = section_start + index * 40
        virtual_size, virtual_address, raw_size, raw_offset = struct.unpack_from(
            "<IIII", data, offset + 8
        )
        sections.append(
            (virtual_address, max(virtual_size, raw_size), raw_size, raw_offset)
        )

    def rva_to_offset(rva: int) -> int:
        for virtual_address, span, raw_size, raw_offset in sections:
            if virtual_address <= rva < virtual_address + span:
                displacement = rva - virtual_address
                if displacement >= raw_size:
                    break
                return raw_offset + displacement
        raise RunnerError(f"Could not map PE RVA {rva:#x} in {path}.")

    descriptor = rva_to_offset(import_rva)
    names: list[str] = []
    max_descriptors = max(1, import_size // 20)
    for _ in range(max_descriptors):
        original_thunk, time_date, forwarder, name_rva, first_thunk = struct.unpack_from(
            "<IIIII", data, descriptor
        )
        if not (original_thunk or time_date or forwarder or name_rva or first_thunk):
            break
        name_offset = rva_to_offset(name_rva)
        end = data.find(b"\0", name_offset)
        if end < 0:
            raise RunnerError(f"Unterminated imported DLL name in {path}.")
        names.append(data[name_offset:end].decode("ascii", errors="replace"))
        descriptor += 20
    return names


SYSTEM_DLLS = {
    "advapi32.dll", "bcrypt.dll", "combase.dll", "comctl32.dll",
    "crypt32.dll", "gdi32.dll", "imm32.dll", "kernel32.dll",
    "msvcp140.dll", "ntdll.dll", "ole32.dll", "oleaut32.dll",
    "sechost.dll", "shell32.dll", "ucrtbase.dll", "user32.dll",
    "vcruntime140.dll", "vcruntime140_1.dll", "version.dll",
    "win32u.dll", "winmm.dll", "ws2_32.dll",
}


def is_os_dll(name: str) -> bool:
    folded = name.casefold()
    return (
        folded in SYSTEM_DLLS
        or folded.startswith("api-ms-win-")
        or folded.startswith("ext-ms-win-")
    )


def runtime_dlls(artifacts: list[TestArtifact]) -> list[Path]:
    """Collect non-system DLL dependencies needed beside the test binaries."""

    target_debug: Path | None = None
    for artifact in artifacts:
        for parent in artifact.executable.parents:
            if parent.name.casefold() == "debug":
                target_debug = parent
                break
        if target_debug is not None:
            break

    search_dirs: list[Path] = []
    for artifact in artifacts:
        search_dirs.append(artifact.executable.parent)
    if target_debug is not None:
        search_dirs.extend([target_debug, target_debug / "deps"])
        search_dirs.extend(path for path in target_debug.glob("build/*/*/out") if path.is_dir())

    try:
        sysroot_result = subprocess.run(
            ["rustc", "-vV"], capture_output=True, text=True, check=True
        )
        rust_host = next(
            line.split(":", 1)[1].strip()
            for line in sysroot_result.stdout.splitlines()
            if line.startswith("host:")
        )
        sysroot = Path(
            subprocess.run(
                ["rustc", "--print", "sysroot"],
                capture_output=True,
                text=True,
                check=True,
            ).stdout.strip()
        )
        rustlib = sysroot / "lib" / "rustlib" / rust_host
        search_dirs.extend([rustlib / "lib", rustlib / "bin"])
    except (OSError, subprocess.CalledProcessError, StopIteration):
        pass

    search_dirs.extend(
        Path(value)
        for value in os.environ.get("PATH", "").split(os.pathsep)
        if value and Path(value).is_dir()
    )
    unique_dirs: list[Path] = []
    seen_dirs: set[str] = set()
    for directory in search_dirs:
        try:
            key = str(directory.resolve()).casefold()
        except OSError:
            continue
        if key not in seen_dirs and directory.is_dir():
            seen_dirs.add(key)
            unique_dirs.append(directory)

    dll_index: dict[str, Path] = {}
    for directory in unique_dirs:
        try:
            for candidate in directory.glob("*.dll"):
                dll_index.setdefault(candidate.name.casefold(), candidate)
        except OSError:
            continue

    pending = [artifact.executable for artifact in artifacts]
    visited_images: set[str] = set()
    found: dict[str, Path] = {}
    while pending:
        image = pending.pop()
        image_key = str(image.resolve()).casefold()
        if image_key in visited_images:
            continue
        visited_images.add(image_key)
        for name in pe_imports(image):
            key = name.casefold()
            if is_os_dll(key) or key in found:
                continue
            dependency = dll_index.get(key)
            if dependency is None:
                raise RunnerError(
                    f"Test binary dependency {name!r} from {image.name} was not "
                    "found beside Cargo outputs or in the Rust toolchain."
                )
            found[key] = dependency
            pending.append(dependency)
    return sorted(found.values(), key=lambda path: path.name.casefold())


def stream_local_process(
    command: list[str],
    *,
    env: dict[str, str],
    timeout: int,
    label: str,
    logger: RunLog,
    mirror: Path,
    cwd: Path | None = None,
) -> int:
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
        bufsize=1,
    )
    assert process.stdout is not None
    lines: queue.Queue[str | None] = queue.Queue()

    def read_output() -> None:
        for line in process.stdout:
            lines.put(line.rstrip("\r\n"))
        lines.put(None)

    reader = threading.Thread(target=read_output, daemon=True)
    reader.start()
    started = time.monotonic()
    deadline = started + timeout
    last_output = started
    finished_reading = False
    while process.poll() is None or not finished_reading:
        now = time.monotonic()
        if now >= deadline:
            terminate_process_tree(process)
            reader.join(timeout=5)
            close_process_output(process)
            raise RunnerError(f"{label} timed out after {timeout}s.")
        try:
            line = lines.get(timeout=min(1, deadline - now))
            if line is None:
                finished_reading = True
            else:
                logger.write(f"[{label}] {line}", mirror=mirror)
                last_output = time.monotonic()
        except queue.Empty:
            if process.poll() is not None:
                finished_reading = True
            elapsed = time.monotonic() - started
            if time.monotonic() - last_output >= 5:
                logger.write(f"[{label}] still running ({elapsed:.0f}s)", mirror=mirror)
                last_output = time.monotonic()
    reader.join(timeout=5)
    close_process_output(process)
    return process.wait()


def copy_file(
    connection: RemoteConnection,
    local: Path,
    remote: str,
    *,
    index: int,
    total: int,
    timeout: int,
    logger: RunLog,
) -> None:
    size = local.stat().st_size
    started = time.monotonic()
    transfer_log = logger.run_dir / "transfer.log"
    prefix = f"[transfer {index}/{total}] {local.name} ({size / 1_048_576:.1f} MB)"
    logger.write(prefix + " started", mirror=transfer_log)

    def progress(message: str) -> None:
        logger.write(f"[transfer {index}/{total}] {message}", mirror=transfer_log)

    connection.upload(local, remote, timeout=timeout, progress=progress)
    elapsed = time.monotonic() - started
    rate = size / 1_048_576 / elapsed if elapsed else 0.0
    logger.write(
        f"[transfer {index}/{total}] {local.name} complete in {elapsed:.1f}s "
        f"({rate:.1f} MB/s)",
        mirror=transfer_log,
    )


def remote_test_task_script(
    executable: str,
    args: list[str],
    remote_log: str,
    task_name: str,
    timeout: int,
    *,
    rustdoc_harness: bool = False,
) -> str:
    """Create a PowerShell controller/worker pair for the active console."""
    exit_path = remote_log + ".exit"
    args_literal = ", ".join(powershell_quote(arg) for arg in args)
    rustdoc_harness_literal = "$true" if rustdoc_harness else "$false"
    return f'''param([switch]$Worker)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$testExecutable = {powershell_quote(executable)}
$testArguments = @({args_literal})
$testLogPath = {powershell_quote(remote_log)}
$testExitPath = {powershell_quote(exit_path)}
$taskName = {powershell_quote(task_name)}
$testTimeoutSeconds = {timeout}
$rustdocHarness = {rustdoc_harness_literal}

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class GlazeRemoteTestSession {{
  [DllImport("kernel32.dll")] public static extern uint WTSGetActiveConsoleSessionId();
  [DllImport("user32.dll")] public static extern int GetSystemMetrics(int index);
  [DllImport("wtsapi32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
  public static extern bool WTSQuerySessionInformation(IntPtr server, uint sessionId, int infoClass, out IntPtr buffer, out int bytes);
  [DllImport("wtsapi32.dll")] public static extern void WTSFreeMemory(IntPtr buffer);
  public static string QueryString(uint id, int infoClass) {{
    IntPtr p; int n;
    if (!WTSQuerySessionInformation(IntPtr.Zero, id, infoClass, out p, out n)) throw new System.ComponentModel.Win32Exception();
    try {{ return Marshal.PtrToStringUni(p); }} finally {{ WTSFreeMemory(p); }}
  }}
  public static int QueryInt(uint id, int infoClass) {{
    IntPtr p; int n;
    if (!WTSQuerySessionInformation(IntPtr.Zero, id, infoClass, out p, out n)) throw new System.ComponentModel.Win32Exception();
    try {{ return Marshal.ReadInt32(p); }} finally {{ WTSFreeMemory(p); }}
  }}
}}
"@

function Read-RemoteTestLogLines([string]$path) {{
  $stream = [System.IO.FileStream]::new(
    $path,
    [System.IO.FileMode]::Open,
    [System.IO.FileAccess]::Read,
    [System.IO.FileShare]::ReadWrite
  )
  try {{
    $reader = [System.IO.StreamReader]::new(
      $stream, [System.Text.UTF8Encoding]::new($false), $true
    )
    try {{
      $lines = [System.Collections.Generic.List[string]]::new()
      while ($null -ne ($line = $reader.ReadLine())) {{ $lines.Add($line) }}
      return $lines.ToArray()
    }} finally {{ $reader.Dispose() }}
  }} finally {{ $stream.Dispose() }}
}}

if ($Worker) {{
  $writer = $null
  $testExitCode = 125
  try {{
    $writer = [System.IO.StreamWriter]::new(
      $testLogPath, $false, [System.Text.UTF8Encoding]::new($false)
    )
    $sessionId = [System.Diagnostics.Process]::GetCurrentProcess().SessionId
    $consoleSessionId = [GlazeRemoteTestSession]::WTSGetActiveConsoleSessionId()
    $monitorCount = [GlazeRemoteTestSession]::GetSystemMetrics(80)
    $writer.AutoFlush = $true
    $writer.WriteLine("[session] process=$sessionId active_console=$consoleSessionId monitors=$monitorCount")
    if ($consoleSessionId -eq [uint32]::MaxValue -or $sessionId -ne $consoleSessionId) {{
      throw "Test task did not start in the active console session (process=$sessionId, console=$consoleSessionId)."
    }}
    if ($monitorCount -lt 1) {{
      throw "The active console session has no visible monitors (session=$sessionId)."
    }}
    if ($rustdocHarness) {{
      $env:RUSTDOC_DOCTEST_BIN_PATH = $testExecutable
    }}
    & $testExecutable @testArguments 2>&1 | ForEach-Object {{ $writer.WriteLine($_.ToString()) }}
    $testExitCode = $LASTEXITCODE
  }} catch {{
    if ($null -ne $writer) {{ $writer.WriteLine("ERROR: $_") }}
    $testExitCode = 125
  }} finally {{
    if ($null -ne $writer) {{ $writer.Dispose() }}
    [System.IO.File]::WriteAllText($testExitPath, [string]$testExitCode)
  }}
  exit $testExitCode
}}

$finalExitCode = 125
$taskRegistered = $false
try {{
  $sessionId = [GlazeRemoteTestSession]::WTSGetActiveConsoleSessionId()
  if ($sessionId -eq [uint32]::MaxValue) {{
    throw 'No active Windows console session was found for GUI tests.'
  }}
  $sessionState = [GlazeRemoteTestSession]::QueryInt($sessionId, 8)
  if ($sessionState -ne 0) {{
    throw "The active Windows console session is disconnected (session=$sessionId, state=$sessionState)."
  }}
  $domain = [GlazeRemoteTestSession]::QueryString($sessionId, 7)
  $user = [GlazeRemoteTestSession]::QueryString($sessionId, 5)
  if ([string]::IsNullOrWhiteSpace($user)) {{
    throw "No logged-on user was found in the active console session (session=$sessionId)."
  }}
  $userSid = (New-Object System.Security.Principal.NTAccount($domain, $user)).Translate(
    [System.Security.Principal.SecurityIdentifier]
  ).Value
  Write-Output "[session] using active console session $sessionId for the test desktop"
  foreach ($path in @($testLogPath, $testExitPath)) {{
    if (Test-Path -LiteralPath $path) {{ Remove-Item -LiteralPath $path -Force }}
  }}
  $powerShell = Join-Path $env:SystemRoot 'System32\\WindowsPowerShell\\v1.0\\powershell.exe'
  $workerArguments = "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker"
  $action = New-ScheduledTaskAction -Execute $powerShell -Argument $workerArguments
  $principal = New-ScheduledTaskPrincipal -UserId $userSid -LogonType Interactive -RunLevel Limited
  $settings = New-ScheduledTaskSettingsSet `
    -ExecutionTimeLimit (New-TimeSpan -Seconds $testTimeoutSeconds) `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
  Register-ScheduledTask -TaskName $taskName -Action $action -Principal $principal -Settings $settings -Force | Out-Null
  $taskRegistered = $true
  Start-ScheduledTask -TaskName $taskName

  $sentLineCount = 0
  $deadline = (Get-Date).AddSeconds($testTimeoutSeconds)
  while (-not (Test-Path -LiteralPath $testExitPath)) {{
    if (Test-Path -LiteralPath $testLogPath) {{
      $lines = @(Read-RemoteTestLogLines $testLogPath)
      while ($sentLineCount -lt $lines.Length) {{
        Write-Output $lines[$sentLineCount]
        $sentLineCount++
      }}
    }}
    if ((Get-Date) -ge $deadline) {{
      throw "Interactive test exceeded its $testTimeoutSeconds second timeout."
    }}
    Start-Sleep -Milliseconds 250
  }}
  if (Test-Path -LiteralPath $testLogPath) {{
    $lines = @(Read-RemoteTestLogLines $testLogPath)
    while ($sentLineCount -lt $lines.Length) {{
      Write-Output $lines[$sentLineCount]
      $sentLineCount++
    }}
  }}
  $exitText = [System.IO.File]::ReadAllText($testExitPath).Trim()
  if (-not [int]::TryParse($exitText, [ref]$finalExitCode)) {{
    throw "The interactive test returned an invalid exit code: $exitText"
  }}
}} catch {{
  Write-Output "ERROR: $_"
  $finalExitCode = 125
}} finally {{
  if ($taskRegistered) {{
    try {{
      $task = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
      if ($null -ne $task -and $task.State -eq 'Running') {{
        Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
      }}
      Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
    }} catch {{ Write-Output "[cleanup] could not remove interactive task: $_" }}
  }}
}}
Write-Output ('{EXIT_MARKER}' + $finalExitCode)
exit $finalExitCode
'''


def remote_test_command(remote_script: str) -> str:
    invocation = f"& {powershell_quote(remote_script)}"
    encoded = base64.b64encode(invocation.encode("utf-16le")).decode("ascii")
    return f"powershell.exe -NoProfile -NonInteractive -EncodedCommand {encoded}"


def kill_remote_tree(
    connection: RemoteConnection,
    image_name: str,
    task_name: str,
    *,
    timeout: int,
    logger: RunLog,
) -> None:
    command = (
        "powershell.exe -NoProfile -NonInteractive -Command "
        f"\"taskkill.exe /IM {powershell_quote(image_name)} /T /F "
        "2>$null | Out-Null; "
        f"$task = Get-ScheduledTask -TaskName {powershell_quote(task_name)} "
        "-ErrorAction SilentlyContinue; "
        "if ($task) { Stop-ScheduledTask -TaskName $task.TaskName "
        "-ErrorAction SilentlyContinue; Unregister-ScheduledTask "
        "-TaskName $task.TaskName -Confirm:$false -ErrorAction SilentlyContinue }; "
        "exit 0\""
    )
    try:
        result = connection.run_capture(command, timeout=timeout)
        logger.write(
            f"[cleanup] remote taskkill finished with SSH exit {result.returncode}"
        )
    except Exception as error:
        logger.write(f"[cleanup] remote process-tree kill failed: {error}")


def run_remote_test(
    connection: RemoteConnection,
    executable: str,
    image_name: str,
    args: list[str],
    remote_script: str,
    task_name: str,
    *,
    timeout: int,
    index: int,
    total: int,
    local_log: Path,
    logger: RunLog,
) -> RemoteTestOutcome:
    label = f"test {index}/{total} {Path(executable).name}"
    command = remote_test_command(remote_script)
    test_filter = next(
        (argument for argument in args if not argument.startswith("--")),
        "all tests",
    )
    logger.write(f"[{label}] started; filter={test_filter}")
    process = subprocess.Popen(
        connection.ssh_argv(command),
        env=connection.env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
        bufsize=1,
    )
    assert process.stdout is not None
    lines: queue.Queue[str | None] = queue.Queue()

    def read_output() -> None:
        for line in process.stdout:
            lines.put(line.rstrip("\r\n"))
        lines.put(None)

    reader = threading.Thread(target=read_output, daemon=True)
    reader.start()
    started = time.monotonic()
    deadline = started + timeout
    last_output = started
    remote_exit_markers: list[str] = []
    test_summary: dict[str, int] | None = None
    finished_reading = False
    timed_out = False
    while process.poll() is None or not finished_reading:
        now = time.monotonic()
        if now >= deadline:
            timed_out = True
            logger.write(f"[{label}] timed out after {timeout}s; killing remote tree")
            kill_remote_tree(
                connection,
                image_name,
                task_name,
                timeout=30,
                logger=logger,
            )
            terminate_process_tree(process)
            break
        try:
            line = lines.get(timeout=min(1, deadline - now))
            if line is None:
                finished_reading = True
                continue
            if line.startswith(EXIT_MARKER):
                remote_exit_markers.append(line[len(EXIT_MARKER) :].strip())
                logger.write(f"[{label}] received remote exit marker")
            else:
                readable_line = clean_remote_output_line(line)
                if readable_line is not None:
                    logger.write(f"[{label}] {readable_line}", mirror=local_log)
                    parsed_summary = parse_test_summary(readable_line)
                    if parsed_summary is not None:
                        test_summary = parsed_summary
            last_output = time.monotonic()
        except queue.Empty:
            if process.poll() is not None:
                finished_reading = True
            elapsed = time.monotonic() - started
            if time.monotonic() - last_output >= 5:
                logger.write(f"[{label}] still running ({elapsed:.0f}s)")
                last_output = time.monotonic()

    if timed_out:
        reader.join(timeout=5)
        close_process_output(process)
        return RemoteTestOutcome(exit_code=124, test_summary=test_summary)

    reader.join(timeout=5)
    close_process_output(process)
    ssh_exit = process.wait()
    exit_code = resolve_remote_exit_code(ssh_exit, remote_exit_markers)
    if ssh_exit != 0:
        logger.write(
            f"[{label}] SSH/controller exited {ssh_exit}; remote marker(s)="
            f"{remote_exit_markers!r}"
        )
    elif not remote_exit_markers:
        logger.write(f"[{label}] remote exit marker was missing; treating run as failed")
    elif len(remote_exit_markers) != 1:
        logger.write(
            f"[{label}] expected one remote exit marker, received "
            f"{len(remote_exit_markers)}; treating run as failed"
        )
    elif exit_code == 125:
        logger.write(
            f"[{label}] invalid remote exit marker {remote_exit_markers[0]!r}; "
            "treating run as failed"
        )
    else:
        logger.write(f"[{label}] remote worker exit code {remote_exit_markers[0]}")
    logger.write(f"[{label}] finished with exit {exit_code} in {time.monotonic() - started:.1f}s")
    return RemoteTestOutcome(exit_code=exit_code, test_summary=test_summary)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build GlazeWM tests locally and run their executables on a remote Windows host."
    )
    parser.add_argument(
        "--package",
        help="Build and run one Cargo package (for example: wm-platform). Default: workspace.",
    )
    parser.add_argument(
        "--filter",
        "--test-filter",
        dest="test_filter",
        help="Run tests whose Rust test names contain this substring.",
    )
    parser.add_argument(
        "--test-threads",
        type=int,
        help="Override the configured Rust test-thread count. Default: 1.",
    )
    parser.add_argument(
        "--project-root",
        type=Path,
        help="Checkout to build (default: the checkout containing this script).",
    )
    parser.add_argument(
        "--config",
        type=Path,
        help="Local ignored config file (default: config_local.json in the selected checkout).",
    )
    parser.add_argument(
        "--build-only",
        action="store_true",
        help="Compile local test binaries and retain build logs without contacting the remote host.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Validate config and show the selected local/remote scope without building or connecting.",
    )
    return parser.parse_args()


def main() -> int:
    global CONFIG_ROOT, PROJECT_ROOT, RUN_ROOT

    args = parse_args()
    if args.project_root is not None:
        PROJECT_ROOT = local_path(args.project_root, base=Path.cwd())
    if not (PROJECT_ROOT / "Cargo.toml").is_file():
        raise RunnerError(f"No Cargo.toml found in checkout: {PROJECT_ROOT}")

    RUN_ROOT = PROJECT_ROOT / "Temp" / "remote-tests"
    if args.config is None:
        config_path = PROJECT_ROOT / "config_local.json"
    else:
        config_path = local_path(args.config, base=PROJECT_ROOT)
    CONFIG_ROOT = config_path.parent
    config = load_config(config_path)
    run_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + uuid.uuid4().hex[:8]
    run_dir = RUN_ROOT / run_id
    logger = RunLog(run_dir)
    started = datetime.now(timezone.utc)
    failures = 0
    filter_executables_skipped = 0
    filter_matching_tests = 0
    try:
        logger.write(f"[run] id={run_id}")
        logger.write(f"[run] scope={'workspace' if not args.package else args.package}")
        if args.test_filter:
            logger.write(f"[run] filter={args.test_filter}")
        logger.write(f"[run] config={config_path}")
        if args.dry_run:
            logger.write("[dry-run] config is valid; no build or remote connection started")
            return 0

        timeouts = config.get("timeouts", {})
        build_timeout = int(timeouts.get("build_seconds", 1800))
        transfer_timeout = int(timeouts.get("transfer_seconds", 600))
        test_timeout = int(timeouts.get("test_seconds", 300))
        artifacts = compile_test_binaries(
            args.package, logger, run_dir, timeout=build_timeout
        )
        artifacts_considered = len(artifacts)
        if args.test_filter:
            artifacts, filter_executables_skipped, filter_matching_tests = (
                select_filtered_artifacts(
                    artifacts,
                    args.test_filter,
                    timeout=build_timeout,
                    logger=logger,
                    run_dir=run_dir,
                )
            )
            artifacts_considered = (
                len(artifacts) + filter_executables_skipped
            )
            if not artifacts:
                summary = {
                    "run_id": run_id,
                    "started_utc": started.isoformat(),
                    "finished_utc": datetime.now(timezone.utc).isoformat(),
                    "scope": args.package or "workspace",
                    "filter": args.test_filter,
                    "artifacts_considered": artifacts_considered,
                    "executable_passes": 0,
                    "executable_failures": 0,
                    "executables_skipped_no_match": filter_executables_skipped,
                    "filter_matching_tests": filter_matching_tests,
                    "zero_match_failure": True,
                    "test_cases": {"passed": 0, "failed": 0, "ignored": 0},
                    "remote_run_directory": None,
                }
                (run_dir / "summary.json").write_text(
                    json.dumps(summary, indent=2) + "\n", encoding="utf-8"
                )
                logger.write(
                    "[summary] ZERO-MATCH FAILURE: no runnable test name matched "
                    f"{args.test_filter!r}; logs: {run_dir}"
                )
                return 2
            logger.write(
                "[doctest-build] skipped because --filter selects named "
                "unit/integration tests"
            )
        else:
            artifacts.extend(
                compile_doctest_binaries(
                    args.package, logger, run_dir, timeout=build_timeout
                )
            )
            artifacts_considered = len(artifacts)
        dependencies = runtime_dlls(artifacts)
        logger.write(
            f"[build] found {len(dependencies)} non-system runtime DLL dependencies"
        )

        if args.build_only:
            logger.write(
                f"[run] built {len(artifacts)} test and doctest executables and found "
                f"{len(dependencies)} non-system runtime DLLs; no remote connection used"
            )
            for dependency in dependencies:
                logger.write(f"[build] runtime DLL: {dependency.name}")
            return 0

        connection = RemoteConnection(config)

        remote_root = str(config["remote"]["directory"]).rstrip("/\\")
        if not remote_root:
            raise RunnerError("remote.directory must not be empty.")
        remote_run_dir = remote_path_join(remote_root, run_id)
        prep_command = (
            "powershell.exe -NoProfile -NonInteractive -Command "
            f"\"New-Item -ItemType Directory -Force -Path "
            f"{powershell_quote(remote_run_dir)} | Out-Null\""
        )
        logger.write(
            f"[remote] preparing {remote_run_dir} through "
            f"{config['connection'].get('provider', 'openssh')}"
        )
        prepared = connection.run_capture(
            prep_command,
            timeout=int(timeouts.get("connect_seconds", 30)) + 30,
        )
        if prepared.returncode != 0:
            raise RunnerError(
                "Could not create the remote run directory: "
                + (prepared.stderr or prepared.stdout or f"exit {prepared.returncode}")
            )

        test_threads = args.test_threads
        if test_threads is None:
            test_threads = int(config.get("test_threads", 1))
        if test_threads < 1:
            raise RunnerError("test-threads must be at least 1.")
        test_args = []
        if args.test_filter:
            test_args.append(args.test_filter)
        test_args.append(f"--test-threads={test_threads}")

        uploads: list[tuple[Path, str]] = []
        remote_executables: list[tuple[TestArtifact, str, str, str, str, str]] = []
        for index, artifact in enumerate(artifacts, start=1):
            remote_name = f"{run_id}-test-{index:02d}.exe"
            remote_executable = remote_path_join(remote_run_dir, remote_name)
            uploads.append((artifact.executable, remote_executable))
            image_name = remote_name
            remote_log = remote_path_join(remote_run_dir, f"test-{index:02d}.log")
            remote_script = remote_path_join(
                remote_run_dir, f"test-{index:02d}-interactive.ps1"
            )
            task_name = f"GlazeWMRemoteTest-{run_id}-{index:02d}"
            local_script = run_dir / "remote-scripts" / f"test-{index:02d}.ps1"
            local_script.parent.mkdir(parents=True, exist_ok=True)
            local_script.write_text(
                remote_test_task_script(
                    remote_executable,
                    test_args,
                    remote_log,
                    task_name,
                    test_timeout,
                    rustdoc_harness=(
                        artifact.package == "doctest"
                        and artifact.target.startswith("merged_doctest_")
                    ),
                ),
                encoding="utf-8",
            )
            uploads.append((local_script, remote_script))
            remote_executables.append(
                (
                    artifact,
                    remote_executable,
                    image_name,
                    remote_log,
                    remote_script,
                    task_name,
                )
            )
            logger.write(
                f"[plan] {artifact.package}/{artifact.target} -> {remote_name} "
                "in active Windows desktop session"
            )

        for dependency in dependencies:
            uploads.append((dependency, remote_path_join(remote_run_dir, dependency.name)))

        logger.write(
            f"[transfer] {len(uploads)} files to {remote_run_dir}; "
            f"{sum(path.stat().st_size for path, _ in uploads) / 1_048_576:.1f} MB total"
        )
        for index, (local, remote) in enumerate(uploads, start=1):
            copy_file(
                connection,
                local,
                remote,
                index=index,
                total=len(uploads),
                timeout=transfer_timeout,
                logger=logger,
            )

        logger.write(
            f"[test] running {len(remote_executables)} test executables in the "
            "auto-detected active Windows desktop session"
        )
        executable_passes = 0
        test_executables_with_summary = 0
        test_case_totals = {"passed": 0, "failed": 0, "ignored": 0}
        for index, (
            artifact,
            remote_executable,
            image_name,
            remote_log,
            remote_script,
            task_name,
        ) in enumerate(
            remote_executables, start=1
        ):
            local_log = run_dir / "tests" / f"{index:02d}-{artifact.package}-{artifact.target}.log"
            outcome = run_remote_test(
                connection,
                remote_executable,
                image_name,
                test_args,
                remote_script,
                task_name,
                timeout=test_timeout,
                index=index,
                total=len(remote_executables),
                local_log=local_log,
                logger=logger,
            )
            code = outcome.exit_code
            if outcome.test_summary is not None:
                for key in ("passed", "failed", "ignored"):
                    test_case_totals[key] += outcome.test_summary[key]
                test_executables_with_summary += 1
            elif args.test_filter and code == 0:
                logger.write(
                    f"[test] FAILED {artifact.package}/{artifact.target}: "
                    "filtered run returned success without a libtest summary"
                )
                code = 125
            if (
                args.test_filter
                and code == 0
                and outcome.test_summary is not None
                and outcome.test_summary["passed"] + outcome.test_summary["failed"] == 0
            ):
                logger.write(
                    f"[test] FAILED {artifact.package}/{artifact.target}: "
                    "filtered run executed zero test cases"
                )
                code = 125
            if code != 0:
                failures += 1
                logger.write(f"[test] FAILED {artifact.package}/{artifact.target}: exit {code}")
            else:
                executable_passes += 1
                logger.write(f"[test] PASSED {artifact.package}/{artifact.target}")

        summary = {
            "run_id": run_id,
            "started_utc": started.isoformat(),
            "finished_utc": datetime.now(timezone.utc).isoformat(),
            "scope": args.package or "workspace",
            "filter": args.test_filter,
            "artifacts_considered": artifacts_considered,
            "executable_count": len(remote_executables),
            "executable_passes": executable_passes,
            "executable_failures": failures,
            "executables_skipped_no_match": filter_executables_skipped,
            "filter_matching_tests": filter_matching_tests,
            "test_executables_with_summary": test_executables_with_summary,
            "test_cases": test_case_totals,
            "zero_match_failure": False,
            "remote_run_directory": remote_run_dir,
        }
        (run_dir / "summary.json").write_text(
            json.dumps(summary, indent=2) + "\n", encoding="utf-8"
        )
        logger.write(
            f"[summary] executables: {summary['executable_passes']} passed, "
            f"{summary['executable_failures']} failed, "
            f"{summary['executables_skipped_no_match']} skipped (no match); "
            f"test cases: {test_case_totals['passed']} passed, "
            f"{test_case_totals['failed']} failed, "
            f"{test_case_totals['ignored']} ignored; "
            f"logs: {run_dir}"
        )
        return 1 if failures else 0
    finally:
        logger.close()


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RunnerError as error:
        print(f"remote-tests: {error}", file=sys.stderr)
        raise SystemExit(2) from error
    except KeyboardInterrupt:
        print("remote-tests: interrupted", file=sys.stderr)
        raise SystemExit(130)
