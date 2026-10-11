#!/usr/bin/env python3
"""Run the skill scripts against the public corpus and the binary lookup."""

from __future__ import annotations

import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
SKILL = SCRIPTS.parent
REPO = SKILL.parents[1]
sys.path.insert(0, str(SCRIPTS))

import _mcp  # noqa: E402


def corpus(relative: str) -> Path:
    return REPO / "test-corpus" / relative


def built_binary() -> Path | None:
    override = os.environ.get("PDF_INSPECTOR_MCP_BIN")
    if override:
        path = Path(override)
        if path.is_file():
            return path
    for relative in (
        "target/release/pdf-inspector-mcp",
        "target/debug/pdf-inspector-mcp",
    ):
        path = REPO / relative
        if path.is_file() and os.access(path, os.X_OK):
            return path
    return None


def run_script(name: str, *args: str, env: dict | None = None) -> subprocess.CompletedProcess[str]:
    merged = os.environ.copy()
    if env:
        merged.update(env)
    return subprocess.run(
        [sys.executable, str(SCRIPTS / name), *args],
        check=False,
        capture_output=True,
        text=True,
        env=merged,
    )


class LookupTests(unittest.TestCase):
    def test_env_override_must_exist(self) -> None:
        previous = os.environ.get("PDF_INSPECTOR_MCP_BIN")
        os.environ["PDF_INSPECTOR_MCP_BIN"] = str(SKILL / "missing-binary")
        try:
            with self.assertRaises(_mcp.SkillError) as raised:
                _mcp.find_binary()
            self.assertEqual(raised.exception.exit_code, _mcp.EXIT_NO_BINARY)
        finally:
            if previous is None:
                os.environ.pop("PDF_INSPECTOR_MCP_BIN", None)
            else:
                os.environ["PDF_INSPECTOR_MCP_BIN"] = previous

    def test_bundled_binary_is_preferred_over_path(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            triple = _mcp.target_triples()[0]
            directory = root / "bin" / triple
            directory.mkdir(parents=True)
            binary = directory / _mcp.binary_filename()
            binary.write_text("#!/bin/sh\n", encoding="utf-8")
            binary.chmod(binary.stat().st_mode | stat.S_IEXEC)
            previous = os.environ.pop("PDF_INSPECTOR_MCP_BIN", None)
            try:
                found = _mcp.find_binary(root)
            finally:
                if previous is not None:
                    os.environ["PDF_INSPECTOR_MCP_BIN"] = previous
            self.assertEqual(found, binary)

    def test_missing_binary_exits_4(self) -> None:
        completed = run_script(
            "classify_pdf.py",
            "--path",
            "anything.pdf",
            env={"PDF_INSPECTOR_MCP_BIN": str(SKILL / "missing-binary")},
        )
        self.assertEqual(completed.returncode, 4)
        payload = json.loads(completed.stdout)
        self.assertEqual(payload["code"], "binary_not_found")
        self.assertIn("pdf-inspector-mcp", completed.stderr)
        self.assertNotIn("/home/", completed.stdout)

    def test_help_lists_an_example_and_exits_0(self) -> None:
        for script in (
            "classify_pdf.py",
            "pdf_to_markdown.py",
            "document_to_markdown.py",
            "classify_document.py",
            "identify_tax_form.py",
            "parse_irc_sections.py",
            "split_sec_filing.py",
        ):
            completed = run_script(script, "--help")
            self.assertEqual(completed.returncode, 0, script)
            self.assertIn("Examples:", completed.stdout)
            self.assertIn("--path", completed.stdout)

    def test_missing_path_exits_2(self) -> None:
        completed = run_script("classify_pdf.py")
        self.assertEqual(completed.returncode, 2)


class CorpusTests(unittest.TestCase):
    binary: Path

    @classmethod
    def setUpClass(cls) -> None:
        binary = built_binary()
        if binary is None:
            raise RuntimeError(
                "pdf-inspector-mcp is not built. Set PDF_INSPECTOR_MCP_BIN "
                "or build target/release/pdf-inspector-mcp."
            )
        cls.binary = binary

    def run_corpus(self, script: str, relative: str) -> subprocess.CompletedProcess[str]:
        return run_script(
            script,
            "--path",
            str(corpus(relative)),
            env={"PDF_INSPECTOR_MCP_BIN": str(self.binary)},
        )

    def payload(self, completed: subprocess.CompletedProcess[str]) -> dict:
        self.assertNotIn("/home/", completed.stdout)
        return json.loads(completed.stdout)

    def assert_build(self, body: dict) -> None:
        build = body["build"]
        self.assertEqual(build["server"], "pdf-inspector-mcp")
        self.assertEqual(build["pdf_inspector"], "1.25.0")
        self.assertEqual(build["anydoc"], "0.2.4")
        self.assertTrue(build["version"])
        self.assertTrue(build["git_commit"])
        self.assertRegex(build["target"], r"^[A-Za-z0-9_]+(-[A-Za-z0-9_]+)+$")

    def test_classify_text_pdf(self) -> None:
        completed = self.run_corpus("classify_pdf.py", "source/sample-1.pdf")
        body = self.payload(completed)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(body["route"], "pdf-inspector")
        self.assertEqual(body["result"]["pdf_type"], "TextBased")
        self.assertEqual(body["result"]["page_count"], 4)
        self.assert_build(body)

    def test_classify_scanned_pdf_keeps_one_based_pages(self) -> None:
        completed = self.run_corpus("classify_pdf.py", "scanned/sample-1.pdf")
        body = self.payload(completed)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(body["result"]["pdf_type"], "Scanned")
        self.assertEqual(body["result"]["pages_needing_ocr"], [1])
        self.assertEqual(body["pages_needing_ocr"], [1])
        reasons = body["result"]["ocr_reasons_by_page"]
        self.assertEqual(reasons[0]["page"], 1)
        self.assertTrue(reasons[0]["reasons"])

    def test_pdf_to_markdown_text(self) -> None:
        completed = self.run_corpus("pdf_to_markdown.py", "source/sample-1.pdf")
        body = self.payload(completed)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("§1398", body["result"]["markdown"])
        self.assertEqual(body["build"]["pdf_inspector"], "1.25.0")

    def test_pdf_to_markdown_scanned_exits_3_and_keeps_the_result(self) -> None:
        completed = self.run_corpus("pdf_to_markdown.py", "scanned/sample-1.pdf")
        body = self.payload(completed)
        self.assertEqual(completed.returncode, 3, completed.stdout)
        self.assertEqual(body["code"], "needs_ocr")
        self.assertEqual(body["pages_needing_ocr"], [1])
        self.assertEqual(body["result"]["pages_needing_ocr"], [1])
        self.assertIn("OCR", body["error"])
        self.assertIn("OCR", completed.stderr)
        self.assert_build(body)

    def test_docx_to_markdown(self) -> None:
        completed = self.run_corpus(
            "document_to_markdown.py", "docx/public-fixture.docx"
        )
        body = self.payload(completed)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(body["route"], "anydoc")
        self.assertIn("Public Fixture Heading", body["result"]["markdown"])
        self.assertEqual(body["result"]["provider"]["name"], "anydoc")
        self.assertEqual(body["result"]["provider"]["version"], "0.2.4")
        self.assert_build(body)

    def test_pdf_is_not_sent_through_anydoc(self) -> None:
        completed = self.run_corpus("document_to_markdown.py", "source/sample-1.pdf")
        body = self.payload(completed)
        self.assertEqual(completed.returncode, 1)
        self.assertEqual(body["code"], "use_pdf_tool")
        self.assertEqual(body["classification"]["kind"], "pdf")
        self.assertNotIn("§1398", completed.stdout)

    def test_classify_docx(self) -> None:
        completed = self.run_corpus(
            "classify_document.py", "docx/public-fixture.docx"
        )
        body = self.payload(completed)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(body["result"]["kind"], "docx")
        self.assertIs(body["result"]["enabled"], True)

    def test_tax_form_on_title_26_is_unknown(self) -> None:
        completed = self.run_corpus("identify_tax_form.py", "source/sample-1.pdf")
        body = self.payload(completed)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(body["result"]["form_type"], "unknown")

    def test_tax_form_on_a_scan_is_not_a_silent_unknown(self) -> None:
        completed = self.run_corpus("identify_tax_form.py", "scanned/sample-1.pdf")
        body = self.payload(completed)
        self.assertEqual(completed.returncode, 3)
        self.assertEqual(body["code"], "needs_ocr")
        self.assertIsNone(body["result"])
        self.assertEqual(body["pages_needing_ocr"], [1])
        self.assertNotIn("form_type", body)

    def test_irc_sections(self) -> None:
        completed = self.run_corpus("parse_irc_sections.py", "source/sample-1.pdf")
        body = self.payload(completed)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        numbers = [section["section_number"] for section in body["result"]["sections"]]
        self.assertEqual(numbers, ["§1398", "§1399"])

    def test_sec_split_of_a_non_filing_is_an_empty_list(self) -> None:
        completed = self.run_corpus("split_sec_filing.py", "source/sample-1.pdf")
        body = self.payload(completed)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIsInstance(body["result"], list)

    def test_missing_file(self) -> None:
        completed = self.run_corpus("classify_pdf.py", "source/missing.pdf")
        body = self.payload(completed)
        self.assertEqual(completed.returncode, 1)
        self.assertIn("error", body)
        self.assertNotIn("missing.pdf", body["error"])


if __name__ == "__main__":
    unittest.main()
