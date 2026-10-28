"""Verify the higher-floor pair using the pinned shared pair checker."""
import hashlib
import importlib.util
import json
from pathlib import Path

here = Path(__file__).resolve().parent
record = json.loads((here / 'results.json').read_bytes())
tool = here / record['pair_verifier']
if hashlib.sha256(tool.read_bytes()).hexdigest() != record['pair_verifier_sha256']:
    raise ValueError('Pair verifier changed')
spec = importlib.util.spec_from_file_location('qp_pair_evidence', tool)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
module.verify_study(here)
