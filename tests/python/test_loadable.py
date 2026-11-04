"""End-to-end tests of the built loadable extension.

The Rust test suite covers behaviour in depth against a statically linked
SQLite. These tests check what only the real artifact can: that a stock
SQLite finds the entry point, and that the functions work through the
extension API routing.

Usage: python -m unittest discover -s tests/python  (after `cargo build --release`)
Set SQLITE_JINJA_PATH to test a library somewhere other than target/release.
"""

import os
import pathlib
import sqlite3
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
SUFFIX = {"darwin": "dylib", "win32": "dll"}.get(sys.platform, "so")
PREFIX = "" if sys.platform == "win32" else "lib"
DEFAULT_PATH = ROOT / "target" / "release" / f"{PREFIX}sqlite_jinja"
EXT_PATH = os.environ.get("SQLITE_JINJA_PATH", str(DEFAULT_PATH))


def connect(path=":memory:"):
    conn = sqlite3.connect(path)
    conn.enable_load_extension(True)
    # No entry point given: SQLite must derive sqlite3_sqlitejinja_init.
    conn.load_extension(EXT_PATH)
    conn.enable_load_extension(False)
    return conn


class LoadableExtensionTest(unittest.TestCase):
    def test_inline_render_with_bound_parameters(self):
        conn = connect()
        (out,) = conn.execute(
            "SELECT jinja_render('Hi {{ name }}, {{ items | length }} items', "
            "json_object('name', ?, 'items', json(?)))",
            ("Ada", '["a", "b"]'),
        ).fetchone()
        self.assertEqual(out, "Hi Ada, 2 items")

    def test_generic_entry_point(self):
        conn = sqlite3.connect(":memory:")
        conn.enable_load_extension(True)
        conn.load_extension(EXT_PATH, entrypoint="sqlite3_extension_init")
        (out,) = conn.execute("SELECT jinja_render('ok')").fetchone()
        self.assertEqual(out, "ok")

    def test_errors_surface_as_sqlite_errors(self):
        conn = connect()
        with self.assertRaisesRegex(sqlite3.OperationalError, "syntax error"):
            conn.execute("SELECT jinja_render('{{ x }')").fetchone()

    def test_stored_templates_across_connections(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "db.sqlite")
            a = connect(path)
            a.executescript(
                """
                CREATE TABLE jinja_templates (name TEXT PRIMARY KEY, source TEXT NOT NULL);
                INSERT INTO jinja_templates VALUES
                  ('base.html', '<p>{% block b %}{% endblock %}</p>'),
                  ('page.html', '{% extends "base.html" %}{% block b %}{{ x }}{% endblock %}');
                """
            )
            a.commit()
            render = "SELECT jinja_render_named('page.html', json_object('x', '<b>'))"
            self.assertEqual(a.execute(render).fetchone()[0], "<p>&lt;b&gt;</p>")

            b = sqlite3.connect(path)
            b.execute("UPDATE jinja_templates SET source = '<div>{% block b %}{% endblock %}</div>' WHERE name = 'base.html'")
            b.commit()
            self.assertEqual(a.execute(render).fetchone()[0], "<div>&lt;b&gt;</div>")
            a.close()
            b.close()

    def test_functions_are_per_connection(self):
        connect()
        with self.assertRaisesRegex(sqlite3.OperationalError, "no such function"):
            sqlite3.connect(":memory:").execute("SELECT jinja_render('x')")


if __name__ == "__main__":
    unittest.main()
