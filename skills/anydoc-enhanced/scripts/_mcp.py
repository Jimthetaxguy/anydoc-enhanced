#!/usr/bin/env python3
"""Stdio JSON-RPC client for the local pdf-inspector-mcp binary.

Python only starts that binary, sends tool arguments, and prints JSON.
Parsing, extraction, and the worker process stay in Rust. The same
executable is the MCP server and the bounded worker (`current_exe`
re-executes itself with `--anydoc-worker`), so a bundled copy does not
need ANYDOC_WORKER_BIN.

Exit codes follow the small AnyDoc-style set, plus a distinct code when
the binary cannot be found:

    0  success
    1  conversion or server error
    2  usage error (argparse)
    3  pages need OCR; text from pages that could be read is still returned
    4  pdf-inspector-mcp was not found

`pages_needing_ocr` and `ocr_reasons_by_page[].page` are already 1-based
in the server payload. This client copies them through unchanged.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import select
import shutil
import subprocess
import sys
import time
from pathlib import Path

EXIT_OK = 0
EXIT_ERROR = 1
EXIT_USAGE = 2
EXIT_NEEDS_OCR = 3
EXIT_NO_BINARY = 4

CALL_TIMEOUT_SECONDS = 90
PROVENANCE_TIMEOUT_SECONDS = 10
BINARY_NAME = "pdf-inspector-mcp"
ENV_BIN = "PDF_INSPECTOR_MCP_BIN"

EXIT_HELP = """\
exit codes:
  0  success
  1  conversion or server error
  2  usage error
  3  pages need OCR; readable pages are still in the JSON result
  4  pdf-inspector-mcp binary was not found
"""

_FULLY_UNREADABLE = frozenset({"Scanned", "ImageBased"})


class SkillError(Exception):
    """A failure that already has a JSON code and a process exit code."""

    def __init__(self, message, code, exit_code, build=None, extra=None):
        super().__init__(message)
        self.message = message
        self.code = code
        self.exit_code = exit_code
        self.build = build
        self.extra = extra or {}


def skill_root() -> Path:
    """Directory that contains SKILL.md. Scripts live in its scripts/ folder."""
    return Path(__file__).resolve().parent.parent


def repo_root() -> Path | None:
    candidate = skill_root().parent.parent
    if (candidate / "Cargo.toml").is_file() and (candidate / "crates").is_dir():
        return candidate
    return None


def target_triples() -> list[str]:
    """Rust target triples this machine can use, musl before gnu on Linux.

    The names match the `bin/<target>/` directory in a skill zip. They are
    not computed by adding one to a page number; page indexes are unrelated.
    """
    machine = platform_machine()
    system = sys.platform
    if system.startswith("linux"):
        return [
            f"{machine}-unknown-linux-musl",
            f"{machine}-unknown-linux-gnu",
        ]
    if system == "darwin":
        return [f"{machine}-apple-darwin"]
    if system.startswith("win"):
        return [f"{machine}-pc-windows-msvc"]
    return []


def platform_machine() -> str:
    machine = platform.machine().lower()
    if machine in {"amd64", "x86_64"}:
        return "x86_64"
    if machine in {"arm64", "aarch64"}:
        return "aarch64"
    return machine


def binary_filename() -> str:
    if sys.platform.startswith("win"):
        return BINARY_NAME + ".exe"
    return BINARY_NAME


def find_binary(root: Path | None = None) -> Path:
    """Resolve the server binary.

    Order: PDF_INSPECTOR_MCP_BIN, then bin/<target>/ beside the skill,
    then the wheel install next to this Python, then PATH.
    An explicit env var that does not point at an executable is an error;
    it is not skipped in favor of another copy.
    """
    override = os.environ.get(ENV_BIN)
    if override is not None and override != "":
        path = Path(override)
        if path.is_file() and os.access(path, os.X_OK):
            return path
        raise SkillError(
            f"{ENV_BIN} is set but is not an executable pdf-inspector-mcp file",
            "binary_not_found",
            EXIT_NO_BINARY,
        )

    skill = root if root is not None else skill_root()
    name = binary_filename()
    for triple in target_triples():
        candidate = skill / "bin" / triple / name
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate

    beside_python = Path(sys.executable).resolve().parent / name
    if beside_python.is_file() and os.access(beside_python, os.X_OK):
        return beside_python

    found = shutil.which(name)
    if found:
        return Path(found)

    raise SkillError(
        "pdf-inspector-mcp was not found. Set PDF_INSPECTOR_MCP_BIN, "
        "place the binary in the skill's bin/<target>/ directory, or "
        "install the anydoc-enhanced wheel so the binary is on PATH. "
        "A Rust toolchain is not required when using a release wheel or "
        "skill zip.",
        "binary_not_found",
        EXIT_NO_BINARY,
    )


def read_provenance(binary: Path) -> dict:
    """Ask the binary which build it is. Falls back to a sibling manifest."""
    try:
        completed = subprocess.run(
            [str(binary), "--provenance"],
            check=False,
            capture_output=True,
            timeout=PROVENANCE_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        # subprocess.run kills the child before raising.
        completed = None
    except OSError as exc:
        raise SkillError(
            "failed to execute pdf-inspector-mcp",
            "binary_not_found",
            EXIT_NO_BINARY,
        ) from exc

    if completed is not None and completed.returncode == 0 and completed.stdout:
        try:
            payload = json.loads(completed.stdout.decode("utf-8"))
        except (UnicodeError, json.JSONDecodeError):
            payload = None
        if isinstance(payload, dict) and _provenance_ok(payload):
            return payload

    manifest = binary.parent / "provenance.json"
    if manifest.is_file():
        try:
            payload = json.loads(manifest.read_text(encoding="utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError):
            payload = None
        if isinstance(payload, dict) and _provenance_ok(payload):
            return payload

    raise SkillError(
        "pdf-inspector-mcp did not report build provenance. Use the binary shipped with this skill.",
        "provenance",
        EXIT_ERROR,
    )


def _provenance_ok(payload: dict) -> bool:
    required = ("server", "version", "git_commit", "pdf_inspector", "anydoc", "target")
    return all(isinstance(payload.get(key), str) and payload[key] for key in required)


class _LineReader:
    def __init__(self, stream):
        self._stream = stream
        self._buffer = b""

    def readline(self, timeout: float) -> str:
        deadline = time.monotonic() + timeout
        while b"\n" not in self._buffer:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SkillError(
                    "pdf-inspector-mcp timed out",
                    "worker_timeout",
                    EXIT_ERROR,
                )
            ready, _, _ = select.select([self._stream], [], [], remaining)
            if not ready:
                raise SkillError(
                    "pdf-inspector-mcp timed out",
                    "worker_timeout",
                    EXIT_ERROR,
                )
            chunk = os.read(self._stream.fileno(), 65536)
            if not chunk:
                if self._buffer:
                    line, self._buffer = self._buffer, b""
                    return line.decode("utf-8")
                raise SkillError(
                    "pdf-inspector-mcp exited before answering",
                    "worker_unavailable",
                    EXIT_ERROR,
                )
            self._buffer += chunk
        line, self._buffer = self._buffer.split(b"\n", 1)
        return line.decode("utf-8")


class Session:
    """One MCP server process for one script invocation."""

    def __init__(self, binary: Path, build: dict):
        self.build = build
        env = os.environ.copy()
        env.setdefault("PDF_INSPECTOR_MCP_LOG", "off")
        self._proc = subprocess.Popen(
            [str(binary)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            bufsize=0,
        )
        assert self._proc.stdin is not None
        assert self._proc.stdout is not None
        self._reader = _LineReader(self._proc.stdout)
        self._next_id = 0
        self._stderr = ""
        self._stderr_thread = self._drain_stderr()
        try:
            self._handshake()
        except Exception:
            self.close()
            raise

    def _drain_stderr(self):
        import threading

        def _read() -> None:
            assert self._proc.stderr is not None
            data = self._proc.stderr.read()
            self._stderr = data.decode("utf-8", errors="replace")

        thread = threading.Thread(target=_read, daemon=True)
        thread.start()
        return thread

    def close(self) -> None:
        if self._proc.stdin is not None:
            self._proc.stdin.close()
        try:
            self._proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self._proc.kill()
            self._proc.wait(timeout=5)
        self._stderr_thread.join(timeout=1)

    def __enter__(self) -> Session:
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()

    def _send(self, message: dict) -> None:
        assert self._proc.stdin is not None
        payload = (json.dumps(message, ensure_ascii=False) + "\n").encode("utf-8")
        try:
            self._proc.stdin.write(payload)
            self._proc.stdin.flush()
        except BrokenPipeError as exc:
            raise SkillError(
                "pdf-inspector-mcp exited before answering",
                "worker_unavailable",
                EXIT_ERROR,
                build=self.build,
            ) from exc

    def _response(self, request_id: int) -> dict:
        while True:
            line = self._reader.readline(CALL_TIMEOUT_SECONDS).strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if message.get("id") == request_id:
                return message

    def _handshake(self) -> None:
        self._next_id += 1
        request_id = self._next_id
        self._send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "anydoc-enhanced-skill", "version": "0.1.0"},
                },
            }
        )
        response = self._response(request_id)
        if response.get("error"):
            raise SkillError(
                "MCP initialize failed",
                "worker_protocol",
                EXIT_ERROR,
                build=self.build,
            )
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def call(self, name: str, arguments: dict):
        self._next_id += 1
        request_id = self._next_id
        self._send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments},
            }
        )
        response = self._response(request_id)
        if response.get("error"):
            detail = response["error"]
            message = detail.get("message") if isinstance(detail, dict) else "MCP request failed"
            raise SkillError(str(message), "worker_protocol", EXIT_ERROR, build=self.build)
        result = response.get("result")
        if not isinstance(result, dict):
            raise SkillError(
                "MCP tool returned an invalid response",
                "worker_protocol",
                EXIT_ERROR,
                build=self.build,
            )
        content = result.get("content")
        if not isinstance(content, list) or not content:
            raise SkillError(
                "MCP tool returned an invalid response",
                "worker_protocol",
                EXIT_ERROR,
                build=self.build,
            )
        text = content[0].get("text") if isinstance(content[0], dict) else None
        if not isinstance(text, str):
            raise SkillError(
                "MCP tool returned an invalid response",
                "worker_protocol",
                EXIT_ERROR,
                build=self.build,
            )
        if result.get("isError") is True:
            raise SkillError(text, "error", EXIT_ERROR, build=self.build)
        try:
            payload = json.loads(text)
        except json.JSONDecodeError as exc:
            raise SkillError(
                "MCP tool returned non-JSON text",
                "worker_protocol",
                EXIT_ERROR,
                build=self.build,
            ) from exc
        error = _tool_error(payload)
        if error is not None:
            code = str(error.get("code") or "error")
            exit_code = EXIT_NEEDS_OCR if code == "needs_ocr" else EXIT_ERROR
            extra = {key: value for key, value in error.items() if key != "error"}
            raise SkillError(
                str(error.get("error") or "tool failed"),
                code,
                exit_code,
                build=self.build,
                extra=extra,
            )
        return payload


def _tool_error(payload) -> dict | None:
    """Server failures are JSON objects with an `error` string.

    Successful PDF objects do not use that key. SEC splits are arrays.
    Page lists inside a success object are left for the caller.
    """
    if not isinstance(payload, dict) or not isinstance(payload.get("error"), str):
        return None
    if any(
        key in payload
        for key in ("pdf_type", "form_type", "sections", "kind", "markdown", "item_number")
    ):
        return None
    return payload


def connect() -> Session:
    binary = find_binary()
    build = read_provenance(binary)
    return Session(binary, build)


def envelope(tool: str, result, build: dict, route: str) -> dict:
    return {
        "tool": tool,
        "route": route,
        "result": result,
        "build": build,
    }


def _ocr_fields(body: dict, pages, reasons, pdf_type: str, readable: bool) -> None:
    # Copy the server's page numbers. They are already 1-based; do not add 1.
    copied = [page for page in pages]
    body["needs_ocr"] = True
    body["code"] = "needs_ocr"
    body["pages_needing_ocr"] = copied
    if reasons is not None:
        body["ocr_reasons_by_page"] = reasons
    if not copied and pdf_type in _FULLY_UNREADABLE:
        body["error"] = "scanned PDF needs OCR, which is disabled"
        return
    shown = ", ".join(str(page) for page in copied)
    if pdf_type in _FULLY_UNREADABLE and not readable:
        if len(copied) == 1:
            body["error"] = f"scanned PDF needs OCR, which is disabled (page {shown})"
        else:
            body["error"] = f"scanned PDF needs OCR, which is disabled (pages {shown})"
        return
    if len(copied) == 1:
        body["error"] = (
            f"page {shown} needs OCR, which is disabled; "
            "text from pages that could be read is included"
        )
    else:
        body["error"] = (
            f"pages {shown} need OCR, which is disabled; "
            "text from pages that could be read is included"
        )


def _readable_markdown(result) -> bool:
    if not isinstance(result, dict):
        return False
    markdown = result.get("markdown")
    return isinstance(markdown, str) and bool(markdown.strip())


def run_pdf_report(_tool: str, path: str):
    """classify_pdf. A scanned file is a successful classification."""
    with connect() as session:
        result = session.call("classify_pdf", {"path": path})
        body = envelope("classify_pdf", result, session.build, "pdf-inspector")
        pages = result.get("pages_needing_ocr") if isinstance(result, dict) else None
        if isinstance(pages, list) and pages:
            body["needs_ocr"] = True
            body["pages_needing_ocr"] = list(pages)
        return body, EXIT_OK


def run_pdf_markdown(_tool: str, path: str):
    with connect() as session:
        result = session.call("pdf_to_markdown", {"path": path})
        body = envelope("pdf_to_markdown", result, session.build, "pdf-inspector")
        if not isinstance(result, dict):
            return body, EXIT_OK
        pages = result.get("pages_needing_ocr") or []
        if not pages:
            return body, EXIT_OK
        _ocr_fields(
            body,
            pages,
            result.get("ocr_reasons_by_page"),
            str(result.get("pdf_type") or ""),
            _readable_markdown(result),
        )
        return body, EXIT_NEEDS_OCR


def run_pdf_domain(tool: str, path: str):
    """Tax, IRC, and SEC extractors. Fully scanned PDFs are not parsed.

    A scanned file would otherwise come back as an empty section list or an
    unknown form, which looks like a real answer. Mixed files still run the
    extractor so text from readable pages is kept, and exit 3.
    """
    with connect() as session:
        classified = session.call("classify_pdf", {"path": path})
        pages = []
        pdf_type = ""
        reasons = None
        if isinstance(classified, dict):
            pages = list(classified.get("pages_needing_ocr") or [])
            pdf_type = str(classified.get("pdf_type") or "")
            reasons = classified.get("ocr_reasons_by_page")
        if pdf_type in _FULLY_UNREADABLE:
            body = envelope(tool, None, session.build, "pdf-inspector")
            body["classification"] = classified
            _ocr_fields(body, pages, reasons, pdf_type, readable=False)
            return body, EXIT_NEEDS_OCR
        result = session.call(tool, {"path": path})
        body = envelope(tool, result, session.build, "pdf-inspector")
        if pages:
            _ocr_fields(body, pages, reasons, pdf_type, readable=True)
            return body, EXIT_NEEDS_OCR
        return body, EXIT_OK


def run_classify_document(_tool: str, path: str):
    with connect() as session:
        result = session.call("classify_document", {"path": path})
        kind = result.get("kind") if isinstance(result, dict) else None
        route = "pdf-inspector" if kind == "pdf" else "anydoc"
        return envelope("classify_document", result, session.build, route), EXIT_OK


def run_document_markdown(_tool: str, path: str):
    """Office conversion. PDFs are refused here so they stay on pdf-inspector.

    AnyDoc fails a whole PDF when any page needs OCR. pdf-inspector reports
    those pages and still returns text from the pages it could read.
    """
    with connect() as session:
        classified = session.call("classify_document", {"path": path})
        kind = classified.get("kind") if isinstance(classified, dict) else None
        if kind == "pdf":
            raise SkillError(
                "PDFs are read with pdf_to_markdown.py. "
                "document_to_markdown does not send PDFs through AnyDoc.",
                "use_pdf_tool",
                EXIT_ERROR,
                build=session.build,
                extra={"classification": classified, "route": "pdf-inspector"},
            )
        result = session.call("document_to_markdown", {"path": path})
        return envelope("document_to_markdown", result, session.build, "anydoc"), EXIT_OK


def write_json(payload: dict) -> None:
    json.dump(payload, sys.stdout, indent=2, ensure_ascii=False)
    sys.stdout.write("\n")


def main(tool: str, description: str, examples: list[str], invoke) -> int:
    parser = argparse.ArgumentParser(
        prog=Path(sys.argv[0]).name,
        description=description,
        epilog="Examples:\n  " + "\n  ".join(examples) + "\n\n" + EXIT_HELP,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--path",
        required=True,
        help="Path to a local file. The file is not uploaded and OCR is not run.",
    )
    args = parser.parse_args()
    try:
        body, code = invoke(tool, args.path)
    except SkillError as exc:
        payload = {
            "tool": tool,
            "error": exc.message,
            "code": exc.code,
            "build": exc.build,
        }
        payload.update(exc.extra)
        write_json(payload)
        print(exc.message, file=sys.stderr)
        return exc.exit_code
    write_json(body)
    if code == EXIT_NEEDS_OCR:
        print(body.get("error", "pages need OCR, which is disabled"), file=sys.stderr)
    return code
