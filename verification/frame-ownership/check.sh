#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
lean_binary=${LEAN_BINARY:-lean}
case "$("$lean_binary" --version)" in
  *'version 4.35.0,'*) ;;
  *) echo 'Lean 4.35.0 is required' >&2; exit 1 ;;
esac
proof_output=$(mktemp -d)
trap 'rm -rf -- "$proof_output"' EXIT
"$lean_binary" -DwarningAsError=true -o "$proof_output/Presentation.olean" Presentation.lean
export LEAN_PATH="$proof_output"
"$lean_binary" -DwarningAsError=true -o "$proof_output/Cancellation.olean" Cancellation.lean
"$lean_binary" -DwarningAsError=true Audit.lean | tee "$proof_output/axioms.log"
python3 - "$proof_output/axioms.log" <<'PY'
import re
import sys
from pathlib import Path

allowed = {'propext', 'Classical.choice', 'Quot.sound'}
expected = {
    'Presentation.all_traces_conserve', 'Presentation.terminal_exactly_one_close',
    'Presentation.cancelled_never_paints', 'Presentation.ack_requires_painted_closed',
    'Presentation.cancelled_trace_terminal', 'Presentation.late_callback_preserves_new',
    'Presentation.finish_idempotent',
}
text = Path(sys.argv[1]).read_text()
reports = re.findall(r"'([^']+)' (depends on axioms: \[([^\]]*)\]|does not depend on any axioms)", text)
if len(reports) != len(expected) or {row[0] for row in reports} != expected:
    raise SystemExit('Missing theorem audit')
for name, _, dependencies in reports:
    actual = {value.strip() for value in dependencies.split(',') if value.strip()}
    if actual - allowed:
        raise SystemExit(f'{name}: unexpected proof dependencies {actual - allowed}')
PY
