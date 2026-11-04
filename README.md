# sqlite-jinja: Jinja2 templates in SQLite

[![CI](https://github.com/thejens/sqlite-jinja/actions/workflows/ci.yml/badge.svg)](https://github.com/thejens/sqlite-jinja/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Linux, macOS, Windows](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey)

**sqlite-jinja is a SQLite extension for rendering Jinja2 templates directly in SQL.**
Generate emails, HTML, Markdown, JSON, prompts or reports from table rows and bound parameters, inside any program that uses SQLite.
It is written in Rust and uses [MiniJinja](https://github.com/mitsuhiko/minijinja), the Jinja2-compatible template engine by the creator of Jinja2.

It is the SQLite counterpart of [pgjinja](https://github.com/thejens/pgjinja) for PostgreSQL. The two share the same function names and template behaviour.

```sql
.load ./libsqlite_jinja

SELECT jinja_render(
  'Hi {{ name }}, you have {{ n }} new {{ "message" if n == 1 else "messages" }}',
  json_object('name', 'Ada', 'n', 3)
);
-- Hi Ada, you have 3 new messages
```

- **Jinja2 syntax you already know:** `{{ }}`, `{% for %}`, `{% if %}`, filters, macros, `{% extends %}`, `{% include %}`, and Python-style methods such as `name.title()`.
- **Render from row data or bound parameters:** pass a JSON context built with `json_object(...)`.
- **Store templates in a table:** stored templates can extend, include and import each other, and edits are picked up by every connection, including other processes.
- **Fast:** templates are compiled once per connection and cached, at about 1–2 µs per rendered row.
- **Safe by design:** HTML/JSON autoescaping, a per-render instruction budget against runaway loops, and no file system or network access.
- **Deterministic**, so `jinja_render` works in generated columns, indexes and CHECK constraints.
- **Works everywhere SQLite does:** the `sqlite3` CLI, Python, Node.js, Datasette, Go, and Rust (as a statically linked library).

## Contents

- [Installation](#installation)
- [Loading the extension](#loading-the-extension)
- [Usage](#usage)
  - [Render a template with row data](#render-a-template-with-row-data)
  - [Render a template with bound parameters](#render-a-template-with-bound-parameters)
  - [Stored templates with inheritance and includes](#stored-templates-with-inheritance-and-includes)
  - [HTML and JSON autoescaping](#html-and-json-autoescaping)
  - [Validate templates with a CHECK constraint](#validate-templates-with-a-check-constraint)
  - [Generated columns and indexes](#generated-columns-and-indexes)
- [Function reference](#function-reference)
- [Template language](#template-language)
- [How stored templates stay fresh](#how-stored-templates-stay-fresh)
- [Performance](#performance)
- [Using it from Rust](#using-it-from-rust)
- [FAQ](#faq)
- [Development](#development)

## Installation

**Prebuilt binaries:** download the archive for your platform from the [releases page](https://github.com/thejens/sqlite-jinja/releases). Builds are provided for Linux (x86_64, ARM64), macOS (Apple Silicon, Intel) and Windows (x86_64).

**From source:** with a [Rust toolchain](https://rustup.rs):

```sh
git clone https://github.com/thejens/sqlite-jinja.git
cd sqlite-jinja
cargo build --release
# -> target/release/libsqlite_jinja.so (Linux), .dylib (macOS), sqlite_jinja.dll (Windows)
```

## Loading the extension

SQLite derives the entry point from the file name, so you don't need to name it.

**sqlite3 CLI**

```sql
.load ./libsqlite_jinja
```

**Python**

```python
import sqlite3

conn = sqlite3.connect("app.db")
conn.enable_load_extension(True)
conn.load_extension("./libsqlite_jinja")
conn.enable_load_extension(False)

conn.execute("SELECT jinja_render('Hello {{ who }}', json_object('who', ?))", ("world",)).fetchone()
```

**Node.js** ([better-sqlite3](https://github.com/WiseLibs/better-sqlite3))

```js
const db = require("better-sqlite3")("app.db");
db.loadExtension("./libsqlite_jinja");
```

**Datasette**

```sh
datasette app.db --load-extension=./libsqlite_jinja
```

Extensions are loaded per connection, so load it on every connection that uses the functions.

> **macOS:** Apple's bundled `sqlite3` and the system Python are built without extension loading. Use Homebrew's `sqlite` / `python`, or another distribution.

## Usage

### Render a template with row data

Build the context with `json_object`. Columns that hold JSON text go through `json(...)`, so they are embedded as structured values rather than strings:

```sql
CREATE TABLE users (id INTEGER, name TEXT, tags TEXT);
INSERT INTO users VALUES (1, 'Ada', '["math", "poetry"]');

SELECT jinja_render(
  '#{{ id }} {{ name }}: {{ tags | join(", ") }}',
  json_object('id', id, 'name', name, 'tags', json(tags))
) FROM users;
-- #1 Ada: math, poetry
```

Templates can also come from a column, so each row can carry its own template:

```sql
SELECT jinja_render(n.template, json_object('name', u.name))
FROM notifications n JOIN users u ON u.id = n.user_id;
```

### Render a template with bound parameters

```python
conn.execute(
    "SELECT jinja_render(?, json_object('name', ?, 'items', json(?)))",
    ("{{ name }} ordered {{ items | length }} items", "Grace", '["a", "b"]'),
).fetchone()
# ('Grace ordered 2 items',)
```

Nested JSON works as you would expect: `{{ user.address.city }}`, `{% for k, v in settings.items() %}`.

### Stored templates with inheritance and includes

Create a `jinja_templates` table and render its rows by name with `jinja_render_named`. Stored templates can `{% extends %}`, `{% include %}` and `{% import %}` each other, like files in a Jinja2 template directory:

```sql
CREATE TABLE jinja_templates (name TEXT PRIMARY KEY, source TEXT NOT NULL);

INSERT INTO jinja_templates VALUES
  ('layout.html', '<h1>{% block title %}{% endblock %}</h1>{% block body %}{% endblock %}{% include "footer.html" %}'),
  ('footer.html', '<footer>{{ company }}</footer>'),
  ('macros.html', '{% macro badge(t) %}<i>{{ t }}</i>{% endmacro %}'),
  ('page.html',   '{% extends "layout.html" %}{% from "macros.html" import badge %}{% block title %}{{ title }}{% endblock %}{% block body %}{{ badge(tag) }}{% endblock %}');

SELECT jinja_render_named('page.html', json_object('title', 'Hi <you>', 'tag', 'new', 'company', 'A&B'));
-- <h1>Hi &lt;you&gt;</h1><i>new</i><footer>A&amp;B</footer>
```

Edits to `jinja_templates` from any connection or process take effect from the next statement. See [How stored templates stay fresh](#how-stored-templates-stay-fresh).

### HTML and JSON autoescaping

Inline templates are not escaped by default. Pass `1` as the fourth argument (`autoescape`) to turn escaping on:

```sql
SELECT jinja_render('<p>{{ c }}</p>', json_object('c', '<script>'), 0, 1);
-- <p>&lt;script&gt;</p>
```

Stored templates follow the Jinja2 convention of choosing the escaping mode from the template name:

| Name ends in | Escaping |
|---|---|
| `.html`, `.htm`, `.xml` | HTML |
| `.json`, `.json5`, `.js`, `.yaml`, `.yml` | JSON (values are serialized as JSON) |
| anything else | none |

Use `{{ value | safe }}` to opt out for trusted markup.

### Validate templates with a CHECK constraint

```sql
CREATE TABLE email_templates (
  name TEXT PRIMARY KEY,
  body TEXT NOT NULL CHECK (jinja_is_valid(body))
);

SELECT jinja_syntax_error('{% for x in %}');
-- syntax error: unexpected end of block (in <string>:1)
```

A CHECK constraint that calls an extension function makes the table unwritable on connections that haven't loaded the extension. Use it where every writer loads sqlite-jinja.

### Generated columns and indexes

`jinja_render` is registered as deterministic:

```sql
CREATE TABLE contacts (
  data    TEXT,  -- JSON
  display TEXT GENERATED ALWAYS AS (jinja_render('{{ last | upper }}, {{ first }}', data)) STORED
);
CREATE INDEX contacts_by_label ON contacts (jinja_render('{{ last }}', data));
```

## Function reference

| Function | Description |
|---|---|
| `jinja_render(template [, context [, strict [, autoescape]]])` | Renders an inline template. Deterministic and innocuous. |
| `jinja_render_named(name [, context [, strict]])` | Renders a template from `jinja_templates`, resolving `extends`, `include` and `import` from the same table. |
| `jinja_is_valid(template)` | `1` if the template parses, else `0`. |
| `jinja_syntax_error(template)` | The parse error, or `NULL` if the template is valid. |
| `jinja_set_fuel(n)` | Sets this connection's per-render instruction budget (`0` = unlimited; default 10,000,000) and returns the previous value. Only callable directly, not from views or triggers. |

Arguments:

- `context` is JSON text holding an object; `NULL` or omitted means an empty context.
- For SQLite's binary JSONB format, pass `json(blob)`.
- A `NULL` template or name returns `NULL`.
- `strict` and `autoescape` are booleans (`0`/`1`).
- With `strict = 1`, referencing an undefined variable raises an error instead of rendering an empty string (Jinja2's `StrictUndefined`).

The fuel limit exists because a render doesn't return control to SQLite until it finishes. `sqlite3_interrupt()` can't stop a template like `{% for i in range(10**9) %}`, but fuel can.

## Template language

sqlite-jinja uses MiniJinja, which implements nearly all of Jinja2:
- variables, filters and tests
- `if`, `for` (including `loop.*`, `break`, `continue`) and `set`
- macros, `call` blocks, and template inheritance with `extends`, `block`, `include`, `import` and `from`
- whitespace control and expressions

See the [MiniJinja syntax documentation](https://docs.rs/minijinja/latest/minijinja/syntax/index.html) and its [list of differences from Jinja2](https://github.com/mitsuhiko/minijinja/blob/main/COMPATIBILITY.md).

Available on top of the built-in filters:
- Python-style methods on strings, lists and dicts (`.upper()`, `.split()`, `.items()`, `.get()`, …)
- `tojson`, `urlencode`, `pluralize`, `truncate` (Jinja2-compatible positional arguments), `striptags`, `filesizeformat`
- `datetimeformat`, `dateformat` and `timeformat`
- `cycler([...])` and `joiner()`

Deliberately not available, to keep rendering deterministic: `now()` and random-number helpers. Pass `datetime('now')` from SQL in the context if you need it.

## How stored templates stay fresh

SQLite has no way to tell other connections that a table changed, so sqlite-jinja checks instead of waiting to be told:

- Each cached stored template remembers the source it was compiled from.
- The **first time a statement renders** a template, every template involved (including partials from `extends`, `include` and `import`) is re-read and compared with that source. Anything that changed is recompiled.
- The check is recorded in SQLite's per-statement auxiliary data for the name argument, so **the remaining rows of that statement render with no table access at all**. An `INSERT … SELECT jinja_render_named(...)` over a million rows reads `jinja_templates` once, not a million times.
- When the template name differs from row to row, SQLite drops the per-statement record and each row is checked.

This picks up changes from every source:
- the same connection, including uncommitted changes inside a transaction and rollbacks;
- other connections;
- other processes.

It needs no triggers or extra tables. Each statement sees one consistent version of the templates.

`jinja_render` has no access to `jinja_templates`, so `{% include %}` in an inline template fails with "template not found". That separation is what lets `jinja_render` be deterministic.

## Performance

Measured on an Apple M-series laptop with SQLite 3.54, release build, 200,000 rows:

| Query | Time | Per row |
|---|---|---|
| `printf('<p>#%d %s</p>', id, customer)` baseline | 35 ms | 0.2 µs |
| `jinja_render(...)` with a 2-key `json_object` | 187 ms | 0.9 µs |
| `jinja_render_named(...)` with `extends` and `block` | 381 ms | 1.9 µs |

## Using it from Rust

Build without the loadable-extension entry points and with SQLite statically linked, then register the functions on your own [rusqlite](https://github.com/rusqlite/rusqlite) connection:

```toml
[dependencies]
sqlite-jinja = { git = "https://github.com/thejens/sqlite-jinja", default-features = false, features = ["static"] }
```

```rust
let conn = rusqlite::Connection::open("app.db")?;
sqlite_jinja::register(&conn)?;
```

## FAQ

### How do I render a Jinja2 template in SQLite?

Load sqlite-jinja and call `jinja_render(template, json_object(...))`.

### Is there a PostgreSQL version?

Yes: [pgjinja](https://github.com/thejens/pgjinja), with the same functions. It uses `jsonb` contexts and Postgres-native cache invalidation.

### Is it safe to render user-supplied templates?

Templates can't touch the file system, the network or other tables. `jinja_render_named` reads only `jinja_templates`. The fuel limit bounds CPU per render. Fuel does not bound memory: a single expression such as `'x' * 10**9` can still allocate a lot. Treat untrusted templates with the same care as untrusted SQL expressions, and enable autoescaping whenever the output is HTML.

### Does it work with Jinja2 templates from my Python or Ansible project?

Mostly, yes. MiniJinja covers the Jinja2 language. Python-specific extensions, custom filters and Ansible-specific filters are not available.

## Development

```sh
cargo test --no-default-features --features static   # Rust tests, SQLite linked in
cargo build --release                                 # the loadable extension
python -m unittest discover -s tests/python -v        # end-to-end tests of the built extension
```

Releases are built by pushing a `v*` tag; see `.github/workflows/release.yml`.

Code layout:

| File | Responsibility |
|---|---|
| `src/functions.rs` | SQL functions, argument handling, registration |
| `src/inline.rs` | Inline templates and their LRU cache |
| `src/stored.rs` | `jinja_templates` loading and per-statement freshness checks |
| `src/environment.rs` | Shared MiniJinja configuration and filters |
| `tests/functions.rs` | Rust tests |
| `tests/python/` | End-to-end tests of the built extension |

## License

MIT. See [LICENSE](LICENSE).
