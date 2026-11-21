# Team contract (lead-owned; do not edit)

Goal: fastest *exactly correct* pipeline for the Shopping Sessions challenge.
`rules.md` (repo root) is authoritative. Never edit it. Score = container wall time
(start to exit) on 8 vCPUs / 24 GB, linux/amd64. Leaderboard top is ~0.75 s on 20M events.

## Implementation interface (all implementations)
A command with no required arguments that:
- reads `$DATA_DIR` (events/*.csv, clicks/*.csv, links/*.csv, products.csv, campaigns.csv, manifest.json)
- writes `$OUTPUT_DIR/user_daily/*.parquet` and `$OUTPUT_DIR/campaign_daily/*.parquet` (creates the folders)
- may spill to `$SCRATCH_DIR`; honours `$NCPU`, `$MEM_GB` if useful; logs to stderr only
- exits 0 on success. The harness also passes DATA_DIR and OUTPUT_DIR as the last two argv; ignore or accept them.

| Impl   | Dir (owner)        | Command (from repo root)                         |
| ------ | ------------------ | ------------------------------------------------ |
| oracle | `oracle/`          | `.venv/bin/python oracle/oracle.py`              |
| duckdb | `duck/`            | `.venv/bin/python duck/pipeline.py`              |
| rust   | `engine/`          | `engine/target/release/shopping-engine`          |

## Shared tools
- Fixtures (36 official, hand-written): `fixtures/<case>/{input,expected}`; see `fixtures/README.md`.
- Run fixtures: `.venv/bin/python scripts/run_fixtures.py fixtures -- <command>`
- Extra adversarial fixtures (same layout): `fixtures_extra/` (owner: spec/oracle agent; QA adds under `fixtures_extra/qa_*`).
- Compare two outputs (parquet folder or folder of expected CSVs): `.venv/bin/python scripts/compare.py A B`
- Python env: `.venv/` (polars, duckdb 1.5.6, pyarrow). Add packages with `uv pip install --python .venv/bin/python ...`.
- Dev set (1M events, seed 1): `data/dev/` (may not exist yet: lead is arranging generation), answer in `data/dev-answer/`.
- `data/`, `downloads/`, `out/`, `target/` are git-ignored. Never check datasets in. Put scratch outputs under `out/<your-name>/`.
- Do NOT build or run anything in `downloads/` (the official generator there is handled by the lead/user). Reading it is fine.

## Rules for every agent
- Touch only your own directory (plus the files explicitly given to you). No git commits; the lead integrates.
- Never sacrifice correctness for speed. Report measurements, not guesses.
- Every failing case found is preserved as a regression fixture.
- Host is an 8-core arm64 Mac with only 8 GB RAM: be careful with memory in local runs.
