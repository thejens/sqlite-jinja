//! In-process tests against a statically linked SQLite.
//! Run with `cargo test --no-default-features --features static`.

use rusqlite::{Connection, OptionalExtension};

fn conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    sqlite_jinja::register(&conn).unwrap();
    conn
}

fn q(conn: &Connection, sql: &str) -> Option<String> {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[track_caller]
fn assert_err(conn: &Connection, sql: &str, needle: &str) {
    let result: rusqlite::Result<Option<String>> = conn
        .query_row(sql, [], |r| r.get::<_, Option<String>>(0))
        .optional()
        .map(Option::flatten);
    match result {
        Err(e) => assert!(
            e.to_string().contains(needle),
            "error {e:?} lacks {needle:?}"
        ),
        Ok(v) => panic!("expected an error containing {needle:?}, got {v:?} from: {sql}"),
    }
}

fn with_templates(conn: &Connection, rows: &[(&str, &str)]) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS jinja_templates (name TEXT PRIMARY KEY, source TEXT NOT NULL)",
    )
    .unwrap();
    for (name, source) in rows {
        conn.execute(
            "INSERT OR REPLACE INTO jinja_templates VALUES (?1, ?2)",
            [name, source],
        )
        .unwrap();
    }
}

// ----- inline templates -----

#[test]
fn renders_bound_parameters() {
    let c = conn();
    let out: String = c
        .query_row(
            "SELECT jinja_render('Hello {{ name | upper }}, {{ n }} new {{ \"message\" if n == 1 else \"messages\" }}', json_object('name', ?1, 'n', ?2))",
            rusqlite::params!["world", 3],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(out, "Hello WORLD, 3 new messages");
}

#[test]
fn renders_row_fields() {
    let c = conn();
    c.execute_batch(
        "CREATE TABLE users (id INTEGER, name TEXT, tags TEXT);
         INSERT INTO users VALUES (1, 'Ada', '[\"math\",\"poetry\"]');",
    )
    .unwrap();
    assert_eq!(
        q(
            &c,
            "SELECT jinja_render('#{{ id }} {{ name }}: {{ tags | join(\", \") }}', json_object('id', id, 'name', name, 'tags', json(tags))) FROM users"
        ),
        Some("#1 Ada: math, poetry".into())
    );
}

#[test]
fn templates_from_a_column_with_check_constraint() {
    let c = conn();
    c.execute_batch(
        "CREATE TABLE tpl (body TEXT CHECK (jinja_is_valid(body)));
         INSERT INTO tpl VALUES ('{% for x in xs %}{{ loop.index }}={{ x }}{% if not loop.last %},{% endif %}{% endfor %}');",
    )
    .unwrap();
    assert_eq!(
        q(
            &c,
            "SELECT jinja_render(body, '{\"xs\": [\"a\", \"b\"]}') FROM tpl"
        ),
        Some("1=a,2=b".into())
    );
    let err = c
        .execute("INSERT INTO tpl VALUES ('{% if %}')", [])
        .unwrap_err();
    assert!(err.to_string().contains("CHECK constraint failed"), "{err}");
}

#[test]
fn generated_column_and_index() {
    let c = conn();
    c.execute_batch(
        "CREATE TABLE contacts (
           data TEXT,
           display TEXT GENERATED ALWAYS AS (jinja_render('{{ last | upper }}, {{ first }}', data)) STORED
         );
         CREATE INDEX contacts_label ON contacts (jinja_render('{{ last }}', data));
         INSERT INTO contacts (data) VALUES ('{\"first\": \"Ada\", \"last\": \"Lovelace\"}');",
    )
    .unwrap();
    assert_eq!(
        q(&c, "SELECT display FROM contacts"),
        Some("LOVELACE, Ada".into())
    );
}

#[test]
fn lenient_by_default_strict_on_request() {
    let c = conn();
    assert_eq!(
        q(&c, "SELECT jinja_render('[{{ missing }}]')"),
        Some("[]".into())
    );
    assert_err(
        &c,
        "SELECT jinja_render('{{ missing }}', '{}', 1)",
        "undefined value",
    );
}

#[test]
fn null_handling() {
    let c = conn();
    assert_eq!(q(&c, "SELECT jinja_render(NULL, '{}')"), None);
    assert_eq!(q(&c, "SELECT jinja_render('ok', NULL)"), Some("ok".into()));
    assert_eq!(
        q(&c, "SELECT jinja_render('ok', 'null')"),
        Some("ok".into())
    );
    assert_eq!(q(&c, "SELECT jinja_render_named(NULL)"), None);
    assert_eq!(
        c.query_row("SELECT jinja_is_valid(NULL)", [], |r| r
            .get::<_, Option<bool>>(0))
            .unwrap(),
        None
    );
}

#[test]
fn context_validation() {
    let c = conn();
    assert_err(
        &c,
        "SELECT jinja_render('x', '[1,2]')",
        "must be a JSON object, got array",
    );
    assert_err(
        &c,
        "SELECT jinja_render('x', 42)",
        "must be a JSON object, got integer",
    );
    assert_err(&c, "SELECT jinja_render('x', '{oops')", "not valid JSON");
    assert_err(
        &c,
        "SELECT jinja_render('x', jsonb('{}'))",
        "wrap JSONB blobs in json()",
    );
    assert_eq!(
        q(
            &c,
            "SELECT jinja_render('{{ a }}', json(jsonb('{\"a\": 1}')))"
        ),
        Some("1".into())
    );
    assert_err(
        &c,
        "SELECT jinja_render(42)",
        "template must be text, got integer",
    );
}

#[test]
fn pycompat_and_helpers() {
    let c = conn();
    assert_eq!(
        q(
            &c,
            r#"SELECT jinja_render('{{ s.title() }}|{% for k, v in d.items() %}{{ k }}={{ v }};{% endfor %}|{{ d.get("z", "dflt") }}', '{"s": "hello there", "d": {"a": 1, "b": 2}}')"#
        ),
        Some("Hello There|a=1;b=2;|dflt".into())
    );
    assert_eq!(
        q(
            &c,
            r#"SELECT jinja_render('{% for i in range(9) %}{% if i == 3 %}{% break %}{% endif %}{{ i }}{% endfor %}|{{ {"a": [1]} | tojson }}|{{ "a b&c" | urlencode }}|{{ 2 | pluralize }}|{{ 2048 | filesizeformat }}|{{ "<b>x</b>" | striptags }}|{{ "2026-11-04" | dateformat }}')"#
        ),
        Some(r#"012|{"a": [1]}|a%20b%26c|s|2.0 kB|x|Nov 4 2026"#.into())
    );
    assert_eq!(
        q(
            &c,
            r#"SELECT jinja_render('{% set c = cycler(["odd", "even"]) %}{{ c.next() }}{{ c.next() }}{% set j = joiner(",") %}{{ j() }}a{{ j() }}b')"#
        ),
        Some("oddevena,b".into())
    );
}

#[test]
fn truncate_accepts_jinja2_positional_arguments() {
    let c = conn();
    assert_eq!(
        q(
            &c,
            r#"SELECT jinja_render('{{ "hello wonderful world" | truncate(9) }}')"#
        ),
        Some("hello...".into())
    );
    assert_eq!(
        q(
            &c,
            r#"SELECT jinja_render('{{ "hello wonderful world" | truncate(9, true, "~", 0) }}')"#
        ),
        Some("hello wo~".into())
    );
    assert_err(
        &c,
        r#"SELECT jinja_render('{{ "x" | truncate(5, length=3) }}')"#,
        "multiple values for argument",
    );
}

#[test]
fn syntax_errors() {
    let c = conn();
    assert!(
        !c.query_row("SELECT jinja_is_valid('{{ oops')", [], |r| r
            .get::<_, bool>(0))
            .unwrap()
    );
    assert_eq!(
        q(&c, "SELECT jinja_syntax_error('{% for x in %}')"),
        Some("syntax error: unexpected end of block (in <string>:1)".into())
    );
    assert_eq!(q(&c, "SELECT jinja_syntax_error('{{ fine }}')"), None);
    assert_err(&c, "SELECT jinja_render('{{ x }')", "syntax error");
}

#[test]
fn fuel_limits_runaway_templates() {
    let c = conn();
    let sql = "SELECT jinja_render('{% for i in range(100000) %}x{% endfor %}')";
    assert_eq!(q(&c, sql).map(|s| s.len()), Some(100_000));
    assert_eq!(
        c.query_row("SELECT jinja_set_fuel(1000)", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        10_000_000
    );
    assert_err(&c, sql, "ran out of fuel");
    c.query_row("SELECT jinja_set_fuel(0)", [], |r| r.get::<_, i64>(0))
        .unwrap();
    assert_eq!(q(&c, sql).map(|s| s.len()), Some(100_000));
    assert_err(&c, "SELECT jinja_set_fuel(-1)", "fuel must be >= 0");
}

#[test]
fn set_fuel_is_direct_only() {
    let c = conn();
    c.execute_batch("CREATE VIEW sneaky AS SELECT jinja_set_fuel(0) AS f")
        .unwrap();
    assert_err(&c, "SELECT f FROM sneaky", "unsafe use of jinja_set_fuel");
}

#[test]
fn autoescape_is_opt_in_and_cached_per_mode() {
    let c = conn();
    let ctx = r#"'{"s": "<b>&</b>"}'"#;
    assert_eq!(
        q(&c, &format!("SELECT jinja_render('{{{{ s }}}}', {ctx})")),
        Some("<b>&</b>".into())
    );
    assert_eq!(
        q(
            &c,
            &format!("SELECT jinja_render('{{{{ s }}}}', {ctx}, 0, 1)")
        ),
        Some("&lt;b&gt;&amp;&lt;&#x2f;b&gt;".into())
    );
    assert_eq!(
        q(
            &c,
            &format!("SELECT jinja_render('{{{{ s | safe }}}}', {ctx}, 0, 1)")
        ),
        Some("<b>&</b>".into())
    );
}

#[test]
fn inline_templates_cannot_reach_stored_ones() {
    let c = conn();
    with_templates(&c, &[("footer", "bye")]);
    assert_err(
        &c,
        "SELECT jinja_render('{% include \"footer\" %}')",
        "template not found",
    );
}

// ----- stored templates -----

#[test]
fn stored_templates_extend_include_and_import() {
    let c = conn();
    with_templates(
        &c,
        &[
            (
                "layout.html",
                r#"<h1>{% block title %}{% endblock %}</h1>{% block body %}{% endblock %}{% include "footer.html" %}"#,
            ),
            ("footer.html", "<footer>{{ company }}</footer>"),
            (
                "macros.html",
                "{% macro badge(t) %}<i>{{ t }}</i>{% endmacro %}",
            ),
            (
                "page.html",
                r#"{% extends "layout.html" %}{% from "macros.html" import badge %}{% block title %}{{ title }}{% endblock %}{% block body %}{{ badge(tag) }}{% endblock %}"#,
            ),
        ],
    );
    assert_eq!(
        q(
            &c,
            r#"SELECT jinja_render_named('page.html', '{"title": "Hi <you>", "tag": "new", "company": "A&B"}')"#
        ),
        Some("<h1>Hi &lt;you&gt;</h1><i>new</i><footer>A&amp;B</footer>".into())
    );
}

#[test]
fn stored_template_escaping_follows_the_name() {
    let c = conn();
    with_templates(
        &c,
        &[
            ("plain", "{{ s }}"),
            ("doc.html", "{{ s }}"),
            ("data.json", r#"{"s": {{ s }}}"#),
        ],
    );
    let ctx = r#"'{"s": "<a \"q\">"}'"#;
    assert_eq!(
        q(&c, &format!("SELECT jinja_render_named('plain', {ctx})")),
        Some("<a \"q\">".into())
    );
    assert_eq!(
        q(&c, &format!("SELECT jinja_render_named('doc.html', {ctx})")),
        Some("&lt;a &quot;q&quot;&gt;".into())
    );
    assert_eq!(
        q(
            &c,
            &format!("SELECT jinja_render_named('data.json', {ctx})")
        ),
        Some(r#"{"s": "<a \"q\">"}"#.into())
    );
}

#[test]
fn edits_are_visible_to_the_next_statement() {
    let c = conn();
    with_templates(
        &c,
        &[("greet", "v1"), ("wrap", r#"[{% include "greet" %}]"#)],
    );
    assert_eq!(
        q(&c, "SELECT jinja_render_named('wrap')"),
        Some("[v1]".into())
    );
    // Only the included template changes; the cached parent must still
    // pick up the new version.
    c.execute(
        "UPDATE jinja_templates SET source = 'v2' WHERE name = 'greet'",
        [],
    )
    .unwrap();
    assert_eq!(
        q(&c, "SELECT jinja_render_named('wrap')"),
        Some("[v2]".into())
    );
    c.execute("DELETE FROM jinja_templates WHERE name = 'greet'", [])
        .unwrap();
    assert_err(
        &c,
        "SELECT jinja_render_named('wrap')",
        "template not found",
    );
}

#[test]
fn rolled_back_edits_are_forgotten() {
    let c = conn();
    with_templates(&c, &[("t", "committed")]);
    assert_eq!(
        q(&c, "SELECT jinja_render_named('t')"),
        Some("committed".into())
    );
    c.execute_batch("BEGIN; UPDATE jinja_templates SET source = 'uncommitted';")
        .unwrap();
    assert_eq!(
        q(&c, "SELECT jinja_render_named('t')"),
        Some("uncommitted".into())
    );
    c.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        q(&c, "SELECT jinja_render_named('t')"),
        Some("committed".into())
    );
}

#[test]
fn edits_from_another_connection_are_visible() {
    let dir = std::env::temp_dir().join(format!("sqlite-jinja-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.sqlite");
    let _ = std::fs::remove_file(&path);

    let a = Connection::open(&path).unwrap();
    sqlite_jinja::register(&a).unwrap();
    with_templates(&a, &[("t", "v1")]);
    assert_eq!(q(&a, "SELECT jinja_render_named('t')"), Some("v1".into()));

    let b = Connection::open(&path).unwrap();
    b.execute("UPDATE jinja_templates SET source = 'v2'", [])
        .unwrap();
    assert_eq!(q(&a, "SELECT jinja_render_named('t')"), Some("v2".into()));

    drop((a, b));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn templates_are_checked_once_per_statement() {
    let c = conn();
    with_templates(&c, &[("row", "{{ i }}")]);
    c.execute_batch(
        "CREATE TABLE nums AS WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000) SELECT i FROM n;
         CREATE TABLE lookups (n INTEGER);
         INSERT INTO lookups VALUES (0);",
    )
    .unwrap();
    // Count reads of jinja_templates by routing them through a view with a
    // side-effecting function. Swap the table for that view.
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = counter.clone();
    c.create_scalar_function(
        "count_lookup",
        0,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        move |_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(1)
        },
    )
    .unwrap();
    c.execute_batch(
        "ALTER TABLE jinja_templates RENAME TO jinja_templates_data;
         CREATE VIEW jinja_templates AS SELECT name, source FROM jinja_templates_data WHERE count_lookup();",
    )
    .unwrap();

    assert_eq!(
        q(
            &c,
            "SELECT CAST(sum(CAST(jinja_render_named('row', json_object('i', i)) AS INTEGER)) AS TEXT) FROM nums"
        ),
        Some("500500".into())
    );
    let first = counter.swap(0, std::sync::atomic::Ordering::Relaxed);
    // One load on the first row, nothing for the remaining 999.
    assert!(
        first <= 2,
        "jinja_templates was read {first} times for one statement"
    );

    // A new statement verifies again.
    assert_eq!(
        q(&c, "SELECT jinja_render_named('row', '{\"i\": 7}')"),
        Some("7".into())
    );
    assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn per_row_template_names_are_checked_per_row() {
    let c = conn();
    with_templates(&c, &[("a", "A"), ("b", "B")]);
    c.execute_batch("CREATE TABLE picks (n TEXT); INSERT INTO picks VALUES ('a'), ('b'), ('a');")
        .unwrap();
    assert_eq!(
        q(
            &c,
            "SELECT group_concat(jinja_render_named(n), '') FROM picks"
        ),
        Some("ABA".into())
    );
}

#[test]
fn stored_template_errors() {
    let c = conn();
    assert_err(
        &c,
        "SELECT jinja_render_named('t')",
        "create it with: CREATE TABLE jinja_templates",
    );
    with_templates(&c, &[("strict", "{{ missing }}")]);
    assert_err(
        &c,
        "SELECT jinja_render_named('nope')",
        "template not found",
    );
    assert_eq!(
        q(&c, "SELECT jinja_render_named('strict')"),
        Some("".into())
    );
    assert_err(
        &c,
        "SELECT jinja_render_named('strict', '{}', 1)",
        "undefined value",
    );
}

#[test]
fn recursive_rendering_errors_instead_of_deadlocking() {
    let c = conn();
    with_templates(&c, &[("t", "x")]);
    c.execute_batch(
        "ALTER TABLE jinja_templates RENAME TO real_templates;
         CREATE VIEW jinja_templates AS SELECT name, jinja_render_named('t') AS source FROM real_templates;",
    )
    .unwrap();
    assert_err(&c, "SELECT jinja_render_named('t')", "recursive rendering");
}
