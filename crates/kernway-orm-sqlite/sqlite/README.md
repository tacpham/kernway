# Vendored SQLite

The official SQLite amalgamation, compiled into the driver by `build.rs` —
the same engine on Linux, macOS and Windows, nothing to install.

- Version: **3.54.1**
- Source: https://sqlite.org/2026/sqlite-amalgamation-3540100.zip
- SHA3-256 of the zip (as published on sqlite.org/download.html):
  `92281c86e2957f9b9305890f9ac04204caa7b677cc868dc326d942689df28e30`
- Files kept: `sqlite3.c`, `sqlite3.h`, `sqlite3ext.h` (unmodified).
- License: public domain (https://sqlite.org/copyright.html).

To upgrade: download the new amalgamation, check its SHA3-256 against the
download page, replace the three files, update this note, and run the
driver's tests and `acceptance/run.sh sqlite`.
