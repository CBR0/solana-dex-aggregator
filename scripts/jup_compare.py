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

# Curated liquid mints (mainnet): (symbol, mint, decimals).
# Verify before trusting a surprising result.
DEFAULT_MINTS = [
    ("USDC", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", 6),
    ("USDT", "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", 6),
    ("JUP", "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN", 6),
    ("BONK", "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", 5),
    ("WIF", "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm", 6),
    ("RAY", "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R", 6),
    ("JTO", "jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL", 9),
    ("PYTH", "HZ1JovNiVvGrGNiiYvEozEVgZ58xaU3RKwX8eACQBCt3", 6),
    ("mSOL", "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So", 9),
    ("JitoSOL", "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn", 9),
    ("POPCAT", "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr", 9),
    ("TRUMP", "6p6xgHyF7AeE6TZkSmFsko444wqoP15icUSqi2jfGiPN", 6),
    ("WEN", "WENWENvqqNya429ubCdR81ZmD69brwQaaBYY6p3LCpk", 5),
]

# Verdict thresholds (bps vs Jupiter). An honest engine sits slightly UNDER
# a 30-venue aggregator on liquid pairs — beating it by much is a math bug,
# not a win. Small negative deltas are the success metric.
INFLATED_ABOVE = 25    # > +25b: our quote likely lies (mispriced pool / bad math)
LAGGING_BELOW = -150   # < -150b: real coverage/routing gap


def fmt_amount(raw, decimals):
    """Raw integer units -> human amount using the mint's decimals."""
    if decimals is None:
        return f"{raw:,}"
    return f"{raw / 10**decimals:,.6f}"


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
    """Lines: "SYMBOL MINT [DECIMALS]" or bare "MINT". Missing decimals -> raw display."""
    mints = []
    with open(path) as f:
        for line in f:
            parts = line.split()
            if not parts or parts[0].startswith("#"):
                continue
            if len(parts) >= 3:
                mints.append((parts[0], parts[1], int(parts[2])))
            elif len(parts) == 2:
                mints.append((parts[0], parts[1], None))
            else:
                mints.append((parts[0][:8], parts[0], None))
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
    hdr = f"{'pair':<10} {'solroute out':>16} {'jupiter out':>16} {'delta':>9}  {'':<8} {'ms':>5}  route (ours | jup)"
    print(hdr)
    print("-" * len(hdr))

    deltas = []
    clean = inflated = lagging = failures = 0
    inflated_pairs = []
    lagging_pairs = []

    for symbol, mint, decimals in mints:
        ours, our_path, ms = quote_solroute(args.engine, mint, lamports, args.max_hops)
        jup, jup_path = quote_jupiter(mint, lamports)
        time.sleep(args.sleep)

        if ours is None or jup is None:
            failures += 1
            reason = our_path if ours is None else jup_path
            side = "solroute" if ours is None else "jupiter"
            print(f"{symbol:<10} {'-':>16} {'-':>16} {'FAIL':>9}  {'':<8} {side}: {reason}")
            continue

        delta_bps = (ours - jup) / jup * 10_000
        deltas.append(delta_bps)
        # Verdict: an honest engine sits slightly under Jupiter. Beating it
        # by much means our quote is wrong, not better.
        if delta_bps > INFLATED_ABOVE:
            verdict = "INFLATED"
            inflated += 1
            inflated_pairs.append((delta_bps, symbol, our_path, jup_path))
        elif delta_bps < LAGGING_BELOW:
            verdict = "lag"
            lagging += 1
            lagging_pairs.append((delta_bps, symbol, our_path, jup_path))
        else:
            verdict = "ok"
            clean += 1
        print(
            f"{symbol:<10} {fmt_amount(ours, decimals):>16} {fmt_amount(jup, decimals):>16}"
            f" {delta_bps:>+8.1f}bps  {verdict:<8} {ms:>5}  {our_path} | {jup_path}"
        )

    print("-" * len(hdr))
    if deltas:
        print(
            f"\ncorrectness: {clean}/{len(deltas)} ok ({LAGGING_BELOW}b..+{INFLATED_ABOVE}b)  "
            f"{inflated} INFLATED (>{INFLATED_ABOVE}b: our math lies)  "
            f"{lagging} lagging (<{LAGGING_BELOW}b: coverage gap)  {failures} fails"
        )
        print(
            f"delta bps: median {statistics.median(deltas):+.1f}  "
            f"mean {statistics.mean(deltas):+.1f}  "
            f"min {min(deltas):+.1f}  max {max(deltas):+.1f}"
        )
        print("target: every pair 'ok', small negative deltas — honest engines sit just under Jupiter")
        if inflated_pairs:
            print("\ninflated quotes (math bugs live here):")
            for d, sym, op, jp in sorted(inflated_pairs, reverse=True)[:5]:
                print(f"  {sym:<10} {d:+10.1f}bps   ours: {op}   jup: {jp}")
        if lagging_pairs:
            print("\nlagging pairs (coverage gaps live here):")
            for d, sym, op, jp in sorted(lagging_pairs)[:5]:
                print(f"  {sym:<10} {d:+10.1f}bps   ours: {op}   jup: {jp}")
    else:
        print(f"\nno successful comparisons ({failures} failures)")


if __name__ == "__main__":
    main()
