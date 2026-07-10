#!/usr/bin/env python3
"""Compare solroute engine quotes against Jupiter for the same pairs.

Routing-quality benchmark: for each mint, quote SOL -> mint on both engines
and report the delta in bps. Positive delta = solroute beats Jupiter.

Usage:
    # engine must be running (RPC_URL=... cargo run --release --bin solroute-engine)
    python3 scripts/jup_compare.py
    python3 scripts/jup_compare.py --amount 10 --max-hops 3
    python3 scripts/jup_compare.py --mints-file mints.txt   # lines: "SYMBOL MINT" or "MINT"

stdlib only — no pip installs.
"""

import argparse
import json
import statistics
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

WSOL = "So11111111111111111111111111111111111111112"
JUP_URL = "https://lite-api.jup.ag/swap/v1/quote"

# Curated liquid mints (mainnet). Verify before trusting a surprising result.
DEFAULT_MINTS = [
    ("USDC", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
    ("USDT", "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"),
    ("JUP", "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN"),
    ("BONK", "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263"),
    ("WIF", "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm"),
    ("RAY", "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R"),
    ("JTO", "jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL"),
    ("PYTH", "HZ1JovNiVvGrGNiiYvEozEVgZ58xaU3RKwX8eACQBCt3"),
    ("mSOL", "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So"),
    ("JitoSOL", "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn"),
    ("POPCAT", "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr"),
    ("TRUMP", "6p6xgHyF7AeE6TZkSmFsko444wqoP15icUSqi2jfGiPN"),
    ("WEN", "WENWENvqqNya429ubCdR81ZmD69brwQaaBYY6p3LCpk"),
]


def http_json(url, timeout=15):
    req = urllib.request.Request(url, headers={"User-Agent": "solroute-jup-compare"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read())


def quote_solroute(engine, output_mint, amount, max_hops):
    """Returns (out_amount, dex_path, ms) or (None, reason, 0)."""
    params = urllib.parse.urlencode(
        {
            "inputMint": "SOL",
            "outputMint": output_mint,
            "amount": amount,
            "maxHops": max_hops,
        }
    )
    try:
        data = http_json(f"{engine}/quote?{params}")
    except urllib.error.HTTPError as e:
        return None, f"HTTP {e.code}", 0
    except Exception as e:
        return None, str(e), 0
    routes = data.get("routes") or []
    if not routes:
        return None, "no route", data.get("timeTakenMs", 0)
    best = max(routes, key=lambda r: int(r["outputAmount"]))
    path = ">".join(h["dexName"] for h in best["hops"])
    return int(best["outputAmount"]), path, data.get("timeTakenMs", 0)


def quote_jupiter(output_mint, amount):
    """Returns (out_amount, label_path) or (None, reason)."""
    params = urllib.parse.urlencode(
        {
            "inputMint": WSOL,
            "outputMint": output_mint,
            "amount": amount,
            "slippageBps": 50,
        }
    )
    try:
        data = http_json(f"{JUP_URL}?{params}")
    except urllib.error.HTTPError as e:
        return None, f"HTTP {e.code}"
    except Exception as e:
        return None, str(e)
    out = data.get("outAmount")
    if out is None:
        return None, data.get("error", "no outAmount")
    path = ">".join(
        leg["swapInfo"].get("label", "?") for leg in data.get("routePlan", [])
    )
    return int(out), path


def load_mints_file(path):
    mints = []
    with open(path) as f:
        for line in f:
            parts = line.split()
            if not parts or parts[0].startswith("#"):
                continue
            if len(parts) >= 2:
                mints.append((parts[0], parts[1]))
            else:
                mints.append((parts[0][:8], parts[0]))
    return mints


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--engine", default="http://localhost:8080")
    ap.add_argument("--amount", type=float, default=1.0, help="SOL input size")
    ap.add_argument("--max-hops", type=int, default=3)
    ap.add_argument("--mints-file", help='file: "SYMBOL MINT" or "MINT" per line')
    ap.add_argument("--sleep", type=float, default=1.2, help="pause between Jupiter calls (rate limit)")
    args = ap.parse_args()

    lamports = int(args.amount * 1_000_000_000)
    mints = load_mints_file(args.mints_file) if args.mints_file else DEFAULT_MINTS

    try:
        http_json(f"{args.engine}/health", timeout=5)
    except Exception as e:
        sys.exit(f"engine unreachable at {args.engine} ({e}) — start solroute-engine first")

    print(f"\nSOL -> X, {args.amount} SOL in, maxHops={args.max_hops}, {len(mints)} pairs\n")
    hdr = f"{'pair':<10} {'solroute out':>16} {'jupiter out':>16} {'delta':>9}  {'ms':>5}  route (ours | jup)"
    print(hdr)
    print("-" * len(hdr))

    deltas = []
    wins = ties = losses = failures = 0
    worst = []

    for symbol, mint in mints:
        ours, our_path, ms = quote_solroute(args.engine, mint, lamports, args.max_hops)
        jup, jup_path = quote_jupiter(mint, lamports)
        time.sleep(args.sleep)

        if ours is None or jup is None:
            failures += 1
            reason = our_path if ours is None else jup_path
            side = "solroute" if ours is None else "jupiter"
            print(f"{symbol:<10} {'-':>16} {'-':>16} {'FAIL':>9}         {side}: {reason}")
            continue

        delta_bps = (ours - jup) / jup * 10_000
        deltas.append(delta_bps)
        if delta_bps > 5:
            wins += 1
        elif delta_bps < -5:
            losses += 1
            worst.append((delta_bps, symbol, our_path, jup_path))
        else:
            ties += 1
        print(
            f"{symbol:<10} {ours:>16,} {jup:>16,} {delta_bps:>+8.1f}b  {ms:>5}  {our_path} | {jup_path}"
        )

    print("-" * len(hdr))
    if deltas:
        print(
            f"\nquoted {len(deltas)}/{len(mints)}  "
            f"wins {wins} (>+5bps)  ties {ties} (±5bps)  losses {losses} (<-5bps)  fails {failures}"
        )
        print(
            f"delta bps: median {statistics.median(deltas):+.1f}  "
            f"mean {statistics.mean(deltas):+.1f}  "
            f"min {min(deltas):+.1f}  max {max(deltas):+.1f}"
        )
        if worst:
            print("\nworst losses (coverage gaps live here):")
            for d, sym, op, jp in sorted(worst)[:5]:
                print(f"  {sym:<10} {d:+8.1f}bps   ours: {op}   jup: {jp}")
    else:
        print(f"\nno successful comparisons ({failures} failures)")


if __name__ == "__main__":
    main()
