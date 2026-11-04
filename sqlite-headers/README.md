# SQLite 3.37.2 headers

The loadable extension's bindings are generated from these headers, so they
set the oldest SQLite the extension loads into: rusqlite refuses to start on
a SQLite older than the headers it was built against. 3.37.2 is the SQLite of
Ubuntu 22.04 and covers every API the extension uses (SQLITE_INNOCUOUS needs
3.31, SQLITE_DIRECTONLY 3.30).

Copied unmodified from sqlite-amalgamation-3370200.zip
(https://www.sqlite.org/2022/sqlite-amalgamation-3370200.zip). SQLite is in
the public domain.
