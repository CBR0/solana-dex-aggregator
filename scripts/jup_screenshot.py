#!/usr/bin/env python3
"""Screenshot-ready solroute vs Jupiter comparison. Reuses jup_compare quoting."""
import sys, time, argparse
sys.path.insert(0, "scripts")
from jup_compare import DEFAULT_MINTS, quote_solroute, quote_jupiter, http_json, fmt_amount

def short(path):
    parts = path.split(">")
    m = {"Meteora DLMM":"DLMM","Meteora DAMM V1":"DAMM V1","Meteora DAMM V2":"DAMM V2",
         "Raydium CLMM":"Ray CLMM","Raydium AMM V4":"Ray V4","Orca Whirlpool":"Whirlpool","Pumpfun AMM":"Pump"}
    return ">".join(m.get(p, p) for p in parts)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", default="http://localhost:8080")
    ap.add_argument("--amount", type=float, default=1.0)
    ap.add_argument("--max-hops", type=int, default=3)
    ap.add_argument("--sleep", type=float, default=0.8)
    a = ap.parse_args()
    lamports = int(a.amount * 1e9)
    http_json(f"{a.engine}/health", timeout=5)

    rows, deltas, exact = [], [], 0
    for sym, mint, dec in DEFAULT_MINTS:
        ours, opath, ms = quote_solroute(a.engine, mint, lamports, a.max_hops)
        jup, jpath = quote_jupiter(mint, lamports)
        time.sleep(a.sleep)
        if ours is None or jup is None: continue
        d = (ours - jup) / jup * 10_000
        deltas.append(d)
        if abs(d) < 0.05: exact += 1
        rows.append((sym, fmt_amount(ours, dec), fmt_amount(jup, dec), d, short(opath)))

    import statistics
    w_sym = max(len(r[0]) for r in rows)
    w_o = max(len(r[1]) for r in rows) + 1
    w_j = max(len(r[2]) for r in rows) + 1
    w_v = max(len(r[4]) for r in rows)
    title = f"solroute vs Jupiter v6   ·   SOL → token, {a.amount:g} SOL in"
    print(f"\n  {title}\n")
    hdr = f"  {'pair':<{w_sym}}  {'solroute':>{w_o}}  {'jupiter':>{w_j}}  {'Δ bps':>10}   {'venue':<{w_v}}"
    print(hdr); print("  " + "─"*(len(hdr)-2))
    for sym,o,j,d,v in rows:
        mark = "  ✓ exact" if abs(d) < 0.05 else ""
        print(f"  {sym:<{w_sym}}  {o:>{w_o}}  {j:>{w_j}}  {d:>+9.1f}   {v:<{w_v}}{mark}")
    print("  " + "─"*(len(hdr)-2))
    print(f"\n  {len(rows)} pairs · {exact} exact matches · median {statistics.median(deltas):+.1f} bps · "
          f"7 DEXs · no routing APIs, all on-chain\n")

main()
