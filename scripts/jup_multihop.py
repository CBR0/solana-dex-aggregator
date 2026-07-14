#!/usr/bin/env python3
"""Multi-hop showcase: token->token pairs (no direct pool) vs Jupiter."""
import sys, time, json, urllib.parse, urllib.request
sys.path.insert(0, "scripts")
from jup_compare import http_json, fmt_amount

M = {  # symbol -> (mint, decimals)
    "BONK": ("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", 5),
    "WIF":  ("EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm", 6),
    "JTO":  ("jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL", 9),
    "JUP":  ("JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN", 6),
    "RAY":  ("4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R", 6),
    "POPCAT":("7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr", 9),
    "PYTH": ("HZ1JovNiVvGrGNiiYvEozEVgZ58xaU3RKwX8eACQBCt3", 6),
    "USDC": ("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", 6),
}
# exotic->exotic (no direct pool -> forces hub routing)
PAIRS = [("PYTH","BONK"),("RAY","BONK"),("JTO","RAY"),("WIF","JTO"),
         ("BONK","JUP"),("PYTH","WIF"),("RAY","JTO"),("JUP","BONK")]
AMT = {  # ~$50-100 worth, raw units
    "USDC": 100_000_000,        # 100 USDC
    "BONK": 300_000_000_000,    # ~3M BONK
    "WIF":  30_000_000,         # ~30 WIF
    "JTO":  15_000_000_000,     # ~15 JTO
    "JUP":  100_000_000,        # ~100 JUP
    "RAY":  20_000_000,         # ~20 RAY
    "POPCAT": 100_000_000_000,  # ~100 POPCAT
    "PYTH": 400_000_000,        # ~400 PYTH
}
JUP="https://lite-api.jup.ag/swap/v1/quote"

def sr(inm,outm,amt,hops):
    q=urllib.parse.urlencode({"inputMint":inm,"outputMint":outm,"amount":amt,"maxHops":hops})
    d=http_json(f"http://localhost:8080/quote?{q}")
    r=d.get("routes") or []
    if not r: return None,"no route"
    b=max(r,key=lambda x:int(x["outputAmount"]))
    return int(b["outputAmount"]), ">".join(h["dexName"].replace("Meteora ","").replace("Raydium ","Ray ").replace("Orca ","") for h in b["hops"])

def jq(inm,outm,amt):
    q=urllib.parse.urlencode({"inputMint":inm,"outputMint":outm,"amount":amt,"slippageBps":50})
    try: d=http_json(f"{JUP}?{q}")
    except Exception as e: return None,str(e)[:20]
    o=d.get("outAmount")
    if o is None: return None,"none"
    return int(o), ">".join(l["swapInfo"].get("label","?") for l in d.get("routePlan",[]))

print(f"\n  solroute vs Jupiter v6  ·  multi-hop (token → token, no direct pool)\n")
hdr=f"  {'pair':<14} {'in':>14} {'solroute out':>18} {'Δ bps':>9}   ours (hops)"
print(hdr); print("  "+"─"*90)
for a,b in PAIRS:
    (am,ad),(bm,bd)=M[a],M[b]
    amt=AMT[a]
    ours,opath=sr(am,bm,amt,3)
    jup,jpath=jq(am,bm,amt); time.sleep(0.8)
    if ours is None or jup is None:
        print(f"  {a+'→'+b:<14} {fmt_amount(amt,ad):>14} {'FAIL':>18}"); continue
    d=(ours-jup)/jup*10_000
    nh=opath.count(">")+1
    print(f"  {a+'→'+b:<14} {fmt_amount(amt,ad):>14} {fmt_amount(ours,bd):>18} {d:>+8.1f}   {opath}  [{nh}-hop]")
print("  "+"─"*90)
print("\n  all routes discovered by graph search through hub mints — no routing APIs\n")
