# Independent check at 60M, new format: the engine's output against the DuckDB pipeline of the same commit.
python3 -m venv /tmp/venv && /tmp/venv/bin/pip -q install polars duckdb==1.5.6 pyarrow >/dev/null 2>&1
free -g | head -2; df -h /tmp | tail -1
/usr/bin/time -v /tmp/eng /tmp/s5 /tmp/e5 2>&1 | grep -E "Elapsed|Maximum resident|Exit"
( cd /tmp/wt && /usr/bin/time -v /tmp/venv/bin/python duck/pipeline.py /tmp/s5 /tmp/d5 2>&1 | grep -E "Elapsed|Maximum resident|Exit|Error|error" )
echo "engine 60M (seed 5) vs DuckDB: $(cd /tmp/wt && /tmp/venv/bin/python scripts/compare.py /tmp/e5 /tmp/d5 2>&1 | tail -1)"
SE_SLOW=1 /usr/bin/time -v /tmp/eng /tmp/s3 /tmp/slow3 2>&1 | grep -E "Elapsed|Maximum resident"
/tmp/eng /tmp/s3 /tmp/e3; echo "general path 20M vs fast path: $(cd /tmp/wt && /tmp/venv/bin/python scripts/compare.py /tmp/slow3 /tmp/e3 2>&1 | tail -1)"
