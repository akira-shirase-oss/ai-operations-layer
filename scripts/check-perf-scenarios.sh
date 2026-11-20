#!/usr/bin/env bash
# Guard for .github/workflows/perf.yml job selection: every `scenario` option must
# select exactly the job it names ('all' selects all of them). A condition written
# as `!= 'other'` once let scenario=adapter run the full ingest AND control loads.
set -euo pipefail
cd "$(dirname "$0")/.."
python3 - <<'PY'
import re, sys

text = open('.github/workflows/perf.yml').read()

opts = re.search(r'scenario:\n(?:.*\n)*?\s+options: \[([^\]]+)\]', text)
section = text.split('\njobs:\n', 1)
if not opts or len(section) != 2:
    sys.exit('check-perf-scenarios: could not read scenario options or the jobs section')
# Each job block: "  <name>:" at two-space indent; its `if:` is inside the block.
blocks = re.split(r'(?m)^  (?=\w+:)', section[1])
jobs = []
for block in blocks:
    name = re.match(r'(\w+):', block)
    cond = re.search(r'(?m)^    if: (.+)$', block)
    if name and cond:
        jobs.append((name.group(1), cond.group(1).strip()))
if not jobs:
    sys.exit('check-perf-scenarios: no job conditions found')

options = [o.strip() for o in opts.group(1).split(',')]
fail = []
for scenario in options:
    runs = []
    for name, cond in jobs:
        # Evaluate only the grammar this workflow uses: == / != / || / && / !().
        expr = cond.replace('inputs.scenario', repr(scenario))
        expr = expr.replace('&&', ' and ').replace('||', ' or ')
        expr = re.sub(r'!\(', ' not (', expr)
        if re.search(r'[^a-zA-Z0-9_ \'"().=!]', expr):
            sys.exit(f'check-perf-scenarios: unsupported token in {name}: {cond}')
        try:
            if eval(expr, {'__builtins__': {}}, {}):
                runs.append(name)
        except Exception as e:
            sys.exit(f'check-perf-scenarios: cannot evaluate {name}: {cond}: {e}')
    want = [n for n, _ in jobs] if scenario == 'all' else [scenario]
    if runs != want:
        fail.append(f'scenario={scenario!r} runs {runs}, expected {want}')

if fail:
    sys.exit('check-perf-scenarios:\n  ' + '\n  '.join(fail))
print(f'check-perf-scenarios: {len(options)} options select exactly their jobs')
PY
