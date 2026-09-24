#!/usr/bin/env python3
"""Independent decoder for Jupiter perps accounts (Custody / Position / Pool).

Golden-value generator for src/domain/lp/perps.rs tests — written from the
Anchor IDL offsets (scripts/golden/perps_idl_offsets.cjs output), NOT from the
Rust code. Reads a raw JSON-RPC getMultipleAccounts / getAccountInfo response.

Usage: python3 scripts/golden/perps_decode.py tests/fixtures/solana/perps/market_gma.json
"""
import base64
import json
import sys

ALPH = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"

CUSTODY_DISC = bytes([1, 184, 48, 81, 93, 131, 63, 145])
POSITION_DISC = bytes([170, 188, 143, 228, 122, 64, 247, 208])
POOL_DISC = bytes([241, 154, 109, 4, 17, 177, 109, 188])

RATE_POWER = 1_000_000_000
BPS_POWER = 10_000
HOURS_IN_A_YEAR = 24 * 365


def b58(b: bytes) -> str:
    n = int.from_bytes(b, "big")
    out = ""
    while n:
        n, r = divmod(n, 58)
        out = ALPH[r] + out
    pad = len(b) - len(b.lstrip(b"\0"))
    return "1" * pad + out


def u(b, off, n):
    return int.from_bytes(b[off : off + n], "little")


def i(b, off, n):
    return int.from_bytes(b[off : off + n], "little", signed=True)


def pk(b, off):
    return b58(b[off : off + 32])


def div_ceil(a, b):
    q, r = divmod(a, b)
    return q + (1 if r else 0)


def custody(b):
    assert b[:8] == CUSTODY_DISC
    c = {
        "pool": pk(b, 8),
        "mint": pk(b, 40),
        "token_account": pk(b, 72),
        "decimals": b[104],
        "is_stable": b[105],
        "oracle_account": pk(b, 106),
        "oracle_type": b[138],
        "max_price_age_sec": u(b, 147, 4),
        "trade_impact_fee_scalar": u(b, 151, 8),
        "max_leverage": u(b, 175, 8),
        "max_global_long": u(b, 183, 8),
        "max_global_short": u(b, 191, 8),
        "permissions": list(b[199:206]),
        "target_ratio_bps": u(b, 206, 8),
        "fees_reserves": u(b, 214, 8),
        "owned": u(b, 222, 8),
        "locked": u(b, 230, 8),
        "guaranteed_usd": u(b, 238, 8),
        "global_short_sizes": u(b, 246, 8),
        "global_short_avg_price": u(b, 254, 8),
        "cumulative_interest_rate": u(b, 262, 16),
        "funding_last_update": i(b, 278, 8),
        "hourly_funding_dbps": u(b, 286, 8),
        "increase_position_bps": u(b, 296, 8),
        "decrease_position_bps": u(b, 304, 8),
        "max_position_size_usd": u(b, 312, 8),
        "doves_oracle": pk(b, 320),
        "min_rate_bps": u(b, 352, 8),
        "max_rate_bps": u(b, 360, 8),
        "target_rate_bps": u(b, 368, 8),
        "target_utilization_rate": u(b, 376, 8),
        "doves_ag_oracle": pk(b, 384),
        "debt": u(b, 1004, 16),
        "borrow_lend_interests_accured": u(b, 1020, 16),
        "trailing_nonzero": sum(1 for x in b[1060:] if x),
    }
    # Jupiter reference getHourlyBorrowRate (jump curve), exact integers.
    debt = div_ceil(max(c["debt"] - c["borrow_lend_interests_accured"], 0), RATE_POWER)
    owned = c["owned"] + debt
    locked = c["locked"] + debt
    if owned > 0 and locked > 0:
        util = locked * RATE_POWER // owned
        mn, mx, tr, tu = (c["min_rate_bps"], c["max_rate_bps"], c["target_rate_bps"],
                          c["target_utilization_rate"])
        if util <= tu:
            yearly = (div_ceil((tr - mn) * util, tu) + mn) * RATE_POWER // BPS_POWER
        else:
            rate_diff = max(0, mx - tr)
            util_diff = max(0, util - tu)
            denom = max(0, RATE_POWER - tu)
            yearly = (div_ceil(rate_diff * util_diff, denom) + tr) * RATE_POWER // BPS_POWER
        hourly = yearly // HOURS_IN_A_YEAR
    else:
        util, hourly = 0, 0
    c["debt_tokens"] = debt
    c["util_raw"] = util
    c["utilization"] = util / RATE_POWER
    c["hourly_rate_raw"] = hourly
    c["borrow_apr_pct"] = hourly / RATE_POWER * HOURS_IN_A_YEAR * 100
    return c


def position(b):
    assert b[:8] == POSITION_DISC
    return {
        "owner": pk(b, 8),
        "pool": pk(b, 40),
        "custody": pk(b, 72),
        "collateral_custody": pk(b, 104),
        "open_time": i(b, 136, 8),
        "update_time": i(b, 144, 8),
        "side": b[152],
        "price": u(b, 153, 8),
        "size_usd": u(b, 161, 8),
        "collateral_usd": u(b, 169, 8),
        "realised_pnl_usd": i(b, 177, 8),
        "cumulative_interest_snapshot": u(b, 185, 16),
        "locked_amount": u(b, 201, 8),
        "bump": b[209],
        "trailing": b[210:].hex(),
    }


def pool(b):
    assert b[:8] == POOL_DISC
    off = 8
    n = u(b, off, 4)
    off += 4
    name = b[off : off + n].decode()
    off += n
    k = u(b, off, 4)
    off += 4
    custodies = [pk(b, off + 32 * j) for j in range(k)]
    off += 32 * k
    aum = u(b, off, 16)
    off += 16
    off += 16 + 16 + 8  # Limit
    off += 9 * 8  # Fees
    off += 8 + 8 + 8  # PoolApr
    mres = i(b, off, 8)
    return {"name": name, "custodies": custodies, "aum_usd": aum,
            "max_request_execution_sec": mres, "max_request_execution_sec_offset": off}


def liquidation_price(p, market, coll):
    """Jupiter reference getLiquidationPrice (jupiterPerps.ts:284-318), integers."""
    size = p["size_usd"]
    if size == 0 or market["max_leverage"] == 0:
        return None
    scalar = market["trade_impact_fee_scalar"]
    impact = div_ceil(size * BPS_POWER, scalar) if scalar else 0
    close_fee = size * (market["decrease_position_bps"] + impact) // BPS_POWER
    borrow_fee = (coll["cumulative_interest_rate"] - p["cumulative_interest_snapshot"]) * size // RATE_POWER
    max_loss = size * BPS_POWER // market["max_leverage"] + close_fee + borrow_fee
    margin = p["collateral_usd"]
    diff = abs(max_loss - margin) * p["price"] // size
    under = max_loss > margin
    if p["side"] == 1:
        liq = p["price"] + diff if under else p["price"] - diff
    else:
        liq = p["price"] - diff if under else p["price"] + diff
    liq = liq / 1_000_000
    return {"liq": liq if liq > 0 else 0, "borrow_fee_raw": borrow_fee, "close_fee_raw": close_fee,
            "impact_bps": impact, "max_loss_raw": max_loss}


def golden(path):
    """Golden values for tests/fixtures/solana/perps/gma.json (account order
    in meta.json: sol custody, usdc custody, pool, then position PDAs)."""
    d = json.load(open(path))
    vals = d["result"]["value"]
    raw = lambda v: base64.b64decode(v["data"][0])
    sol = custody(raw(vals[0]))
    usdc = custody(raw(vals[1]))
    pl = pool(raw(vals[2]))
    print("slot", d["result"]["context"]["slot"])
    for name, c in (("sol", sol), ("usdc", usdc)):
        print(name, {k: c[k] for k in ("util_raw", "utilization", "hourly_rate_raw", "borrow_apr_pct",
                                        "cumulative_interest_rate", "debt_tokens", "owned", "locked")})
    print("pool", pl)
    for idx, v in enumerate(vals[3:], start=3):
        if v is None:
            print(idx, "absent")
            continue
        p = position(raw(v))
        coll = sol if p["side"] == 1 else usdc
        print(idx, {k: p[k] for k in ("owner", "side", "price", "size_usd", "collateral_usd",
                                      "cumulative_interest_snapshot", "open_time", "update_time",
                                      "realised_pnl_usd")})
        if p["size_usd"]:
            print("   liq", liquidation_price(p, sol, coll))
            print("   accrued_borrow_fee_usd",
                  (coll["cumulative_interest_rate"] - p["cumulative_interest_snapshot"])
                  * p["size_usd"] // RATE_POWER / 1e6)


def main():
    if sys.argv[1] == "--golden":
        return golden(sys.argv[2])
    d = json.load(open(sys.argv[1]))
    res = d["result"]
    values = res["value"] if isinstance(res["value"], list) else [res["value"]]
    print("slot", res["context"]["slot"])
    for v in values:
        if v is None:
            print("absent")
            continue
        b = base64.b64decode(v["data"][0])
        disc = b[:8]
        if disc == CUSTODY_DISC:
            print(json.dumps({"custody": custody(b)}, indent=1))
        elif disc == POSITION_DISC:
            print(json.dumps({"position": position(b)}, indent=1))
        elif disc == POOL_DISC:
            print(json.dumps({"pool": pool(b)}, indent=1))
        else:
            print("unknown", disc.hex(), len(b))


if __name__ == "__main__":
    main()
