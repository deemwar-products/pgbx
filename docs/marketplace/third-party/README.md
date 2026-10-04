# Third-party licenses, pgbx v0.6.1

Rust crates compiled into the v0.6.1 release binaries, for THIRD-PARTY-NOTICES (AWS Marketplace and other listings).

| file | what |
|---|---|
| `v0.6.1-cli-licenses.html` | full license texts, CLI (`pgbx-linux-*`), from `cargo about generate` |
| `v0.6.1-ext-pg17-licenses.html` | full license texts, extension (`pgbx-ext-pg17-linux-*`; other Postgres majors use the same crates) |
| `v0.6.1-cli-crates.json`, `v0.6.1-ext-pg17-crates.json` | name, version, license, repository of every normal (shipped) dependency, Linux targets: 165 (CLI) and 186 (extension) crates |

How it was made: the release builds without a committed `Cargo.lock`, so the dependencies were resolved again at the
`v0.6.1` tag on 2026-10-04 and checked against the release run (37182998996): CI locked the same 218 CLI packages, and
every crate the CI build compiled is in the inventory (the inventory may list a few feature-gated extras).

Copyleft: only `attohttpc` 0.30.1, **MPL-2.0** (file-level; used unmodified, source at https://github.com/sbstp/attohttpc).
No GPL, LGPL, AGPL, SSPL or unknown licenses. pgbx itself is MIT (`LICENSE`). The static Linux CLI also links musl libc
(MIT).
