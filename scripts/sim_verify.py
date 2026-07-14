#!/usr/bin/env python3
"""Verify engine quotes against on-chain simulation.

For each pair: pull the engine's best route, simulate the EXACT route with
min_out = quoted output minus a tight slippage. PASS = the chain delivers
the quote; FAIL_SLIPPAGE = the quote was inflated.

Usage: python3 scripts/sim_verify.py [--slippage-bps 50] [--max-hops 3]
Requires: engine running, RPC_URL exported, sim-route built
(cargo build --release -p solroute-aggregator --bin sim-route).
"""

import argparse
import json
import subprocess
import sys
import tempfile
import urllib.parse
import urllib.request

sys.path.insert(0, "scripts")
from jup_compare import DEFAULT_MINTS, http_json  # reuse the pair list


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", default="http://localhost:8080")
    ap.add_argument("--amount", type=float, default=1.0)
    ap.add_argument("--max-hops", type=int, default=3)
    ap.add_argument("--slippage-bps", type=int, default=50)
    args = ap.parse_args()
    lamports = int(args.amount * 1_000_000_000)

    print(f"\nquote -> simulate, {args.amount} SOL, maxHops={args.max_hops}, "
          f"slippage {args.slippage_bps}bps\n")
    hdr = f"{'pair':<10} {'quoted out':>16}  {'verdict':<14} route"
    print(hdr)
    print("-" * 80)

    # Phase 1: collect all routes from the engine.
    jobs = []
    for symbol, mint, decimals in DEFAULT_MINTS:
        q = urllib.parse.urlencode({
            "inputMint": "SOL", "outputMint": mint,
            "amount": lamports, "maxHops": args.max_hops,
        })
        try:
            data = http_json(f"{args.engine}/quote?{q}")
        except Exception as e:
            print(f"{symbol:<10} {'-':>16}  quote error: {e}")
            continue
        routes = data.get("routes") or []
        if not routes:
            print(f"{symbol:<10} {'-':>16}  no route")
            continue
        best = max(routes, key=lambda r: int(r["outputAmount"]))
        path = ">".join(h["dexName"] for h in best["hops"])
        out = int(best["outputAmount"])
        out_h = f"{out / 10**decimals:,.6f}" if decimals else f"{out:,}"

        with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
            json.dump(best, f)
            jobs.append((symbol, out_h, path, f.name))

    # Phase 2: one sim-route process, cache loads once.
    files = ",".join(j[3] for j in jobs)
    res = subprocess.run(
        ["./target/release/sim-route", files, str(args.slippage_bps)],
        capture_output=True, text=True, timeout=600,
    )
    verdicts = {}
    for line in res.stdout.splitlines():
        if line.startswith("ROUTE="):
            parts = line.split()
            rf = parts[0].replace("ROUTE=", "")
            verdicts[rf] = (parts[1].replace("VERDICT=", ""), line)

    passed = failed = skipped = 0
    for symbol, out_h, path, rf in jobs:
        verdict, line = verdicts.get(rf, ("?", ""))
        detail = ""
        if verdict == "PASS":
            passed += 1
            mark = "✅ PASS"
        elif verdict == "FAIL_SLIPPAGE":
            failed += 1
            mark = "❌ INFLATED"
        elif verdict.startswith("FAIL"):
            failed += 1
            mark = "❌ FAIL"
            detail = line[line.find("err="):][:60]
        else:
            skipped += 1
            mark = "⏭  SKIP"
            detail = line[line.find("reason="):][:60]
        print(f"{symbol:<10} {out_h:>16}  {mark:<14} {path}  {detail}")

    print("-" * 80)
    print(f"\n{passed} passed, {failed} failed, {skipped} skipped "
          f"(skip = no swap builder for a venue yet, e.g. Orca Whirlpool)")


if __name__ == "__main__":
    main()
