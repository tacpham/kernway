# Driver acceptance

Every database driver is accepted against a **real server in Docker**, never
against mocks or another client library's output. One command per database:

```sh
acceptance/run.sh mysql    # -> docs/acceptance/mysql.md
acceptance/run.sh oracle   # -> docs/acceptance/oracle.md
acceptance/run.sh sqlite   # -> docs/acceptance/sqlite.md (embedded: no container)
```

The script creates or reuses the container (data is kept), waits for the
database, prepares accounts and certificates, runs **every** test of the driver
crate one at a time (`--test-threads=1 --nocapture`), and renders the report
from `acceptance/<db>.features`.

## Rules the report enforces

- A feature is ✅ only if every test named as its evidence ran **and** passed
  in this run.
- A test that printed `skip` (missing environment) counts as a failure; so
  does a test named in the catalog that did not run.
- Features not built yet are listed as 🚧 `GAP: …` and counted against
  completeness — they are never hidden. `N/A: …` is only for things the
  database itself does not have.
- Every test of the run is listed at the end with whether it backs a feature.
- The script exits non-zero unless everything that ran passed.

## Catalog format (`<db>.features`)

```
area | feature | test_a, module::tests::test_b
area | feature | GAP: why it is missing
area | feature | N/A: why the database has no such thing
```

Unit tests are named with their module path (`protocol::tests::url_parsing`);
integration tests by function name.

## Adding a database

Write `acceptance/<db>.features` (the full feature list of that database,
gaps included), teach `run.sh` how to start its container and set the test
environment, and keep the report in `docs/acceptance/<db>.md` up to date with
each change to the driver.
