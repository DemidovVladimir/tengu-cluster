// Capture a DLMM fixture from mainnet AND compute the golden expected values
// with the Meteora SDK (@meteora-ag/dlmm) from the SAME getMultipleAccounts
// response — so the Rust decoders are checked against an independent
// implementation at one slot.
//
// Run (network; public RPC only; imports only from the bot's node_modules):
//   NODE_PATH=/Users/vladimirdemidov/development/delta_neutral_bot/node_modules \
//     node scripts/golden/dlmm_capture.js [pool]
//
// Writes tests/fixtures/solana/dlmm/{gma.json,meta.json,golden.json}.
//   gma.json    raw JSON-RPC getMultipleAccounts response (base64, context slot)
//   meta.json   request keys (same order as gma.json values) + roles
//   golden.json SDK-computed expectations (prices, fees, reserves, depth,
//               per-position amounts / claimable fees)
const fs = require("fs");
const path = require("path");
const { Connection, PublicKey, SYSVAR_CLOCK_PUBKEY } = require("@solana/web3.js");
const dlmmMod = require("@meteora-ag/dlmm");
const DLMM = dlmmMod.default || dlmmMod; // module.exports IS the DLMM class
const {
  createProgram,
  decodeAccount,
  wrapPosition,
  deriveBinArray,
  binIdToBinArrayIndex,
  getPriceOfBinByBinId,
  getTotalFee,
  getVariableFee,
  getBaseFee,
  ClockLayout,
} = dlmmMod;
const { unpackMint, unpackAccount } = require("@solana/spl-token");
const BN = require("bn.js");
const Decimal = require("decimal.js");

const RPC = "https://api.mainnet-beta.solana.com";
const PROGRAM = new PublicKey("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");
const POOL = new PublicKey(process.argv[2] || "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6");
const POSITION_V2_DISC_B58 = "LgkNAEYaVX3"; // [117,176,212,199,245,180,133,182]
const OUT = path.join(__dirname, "..", "..", "tests", "fixtures", "solana", "dlmm");

async function rpc(method, params) {
  for (let attempt = 0; attempt < 4; attempt++) {
    const res = await fetch(RPC, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
    });
    if (res.status === 429) {
      await new Promise((r) => setTimeout(r, 2000 * (attempt + 1)));
      continue;
    }
    const j = await res.json();
    if (j.error) throw new Error(method + ": " + JSON.stringify(j.error));
    return j;
  }
  throw new Error(method + ": rate limited");
}

async function gma(keys) {
  if (keys.length > 100) throw new Error("gma > 100 keys");
  return rpc("getMultipleAccounts", [keys.map((k) => k.toBase58()), { encoding: "base64", commitment: "confirmed" }]);
}

function toInfo(v) {
  if (!v) return null;
  return {
    data: Buffer.from(v.data[0], "base64"),
    owner: new PublicKey(v.owner),
    lamports: v.lamports,
    executable: v.executable,
  };
}

async function gpaSlice(offset, length) {
  const j = await rpc("getProgramAccounts", [
    PROGRAM.toBase58(),
    {
      encoding: "base64",
      withContext: true,
      dataSlice: { offset, length },
      filters: [
        { memcmp: { offset: 0, bytes: POSITION_V2_DISC_B58 } },
        { memcmp: { offset: 8, bytes: POOL.toBase58() } },
      ],
    },
  ]);
  return j.result.value;
}

function sharesNonZero(buf) {
  const lower = buf.readInt32LE(7912);
  const upper = buf.readInt32LE(7916);
  const width = upper - lower + 1;
  for (let i = 0; i < Math.min(width, 70); i++) {
    const lo = buf.readBigUInt64LE(72 + 16 * i);
    const hi = buf.readBigUInt64LE(72 + 16 * i + 8);
    if (lo !== 0n || hi !== 0n) return true;
  }
  for (let i = 0; i < width - 70; i++) {
    const o = 8120 + 112 * i;
    if (buf.readBigUInt64LE(o) !== 0n || buf.readBigUInt64LE(o + 8) !== 0n) return true;
  }
  return false;
}

(async () => {
  const connection = new Connection(RPC, "confirmed");
  const program = createProgram(connection);

  // Phase A: pool + position discovery.
  const pairResp = await gma([POOL]);
  const pair = decodeAccount(program, "lbPair", toInfo(pairResp.result.value[0]).data);
  const activeId = pair.activeId;
  console.error("active_id", activeId, "bin_step", pair.binStep);

  const owners = await gpaSlice(40, 32);
  const ranges = await gpaSlice(7912, 8);
  const byKey = new Map();
  for (const o of owners) {
    byKey.set(o.pubkey, { pubkey: o.pubkey, space: o.account.space, owner: new PublicKey(Buffer.from(o.account.data[0], "base64")).toBase58() });
  }
  for (const r of ranges) {
    const b = Buffer.from(r.account.data[0], "base64");
    const e = byKey.get(r.pubkey);
    if (e) {
      e.lower = b.readInt32LE(0);
      e.upper = b.readInt32LE(4);
    }
  }
  const all = [...byKey.values()].filter((e) => e.lower !== undefined);
  const inRange = all.filter((e) => e.lower <= activeId && activeId <= e.upper);
  console.error("positions", all.length, "in range", inRange.length);

  // Pick (A) an owner with 2-3 positions in this pool, >= 1 in range, all
  // with non-zero shares (MultiplePositions + plain amounts), and (B) an
  // extended (> 70 bins, <= 200) in-range position of another owner that is
  // that owner's only position in the pool.
  const byOwner = new Map();
  for (const e of all) {
    if (!byOwner.has(e.owner)) byOwner.set(e.owner, []);
    byOwner.get(e.owner).push(e);
  }
  const isIn = (e) => e.lower <= activeId && activeId <= e.upper;
  const multi = [...byOwner.values()].filter(
    (l) => l.length >= 2 && l.length <= 3 && l.some(isIn) && l.every((e) => e.upper - e.lower + 1 <= 70)
  );
  const extSingles = [...byOwner.values()].filter(
    (l) => l.length === 1 && isIn(l[0]) && l[0].space > 8120 && l[0].upper - l[0].lower + 1 <= 200
  );
  console.error("multi owners", multi.length, "extended singles", extSingles.length);
  async function allNonZero(list) {
    const resp = await gma(list.map((e) => new PublicKey(e.pubkey)));
    return resp.result.value.map(toInfo).every((i) => i && sharesNonZero(i.data));
  }
  let groupA = null;
  for (const l of multi.slice(0, 30)) if (await allNonZero(l)) { groupA = l; break; }
  let groupB = null;
  for (const l of extSingles.slice(0, 30)) if (await allNonZero(l)) { groupB = l; break; }
  if (!groupA || !groupB) throw new Error("no suitable positions found");
  const chosen = { owner: groupA[0].owner, extOwner: groupB[0].owner, list: [...groupA, ...groupB] };
  console.error("chosen", chosen.list.map((e) => `${e.owner} ${e.pubkey} [${e.lower},${e.upper}] ${e.space}`));

  // Phase B: ONE getMultipleAccounts with everything.
  const mintX = pair.tokenXMint;
  const mintY = pair.tokenYMint;
  const idx = new Set();
  for (let b = activeId - 70; b <= activeId + 70; b += 1) idx.add(binIdToBinArrayIndex(new BN(b)).toNumber());
  for (const e of chosen.list) {
    for (let i = binIdToBinArrayIndex(new BN(e.lower)).toNumber(); i <= binIdToBinArrayIndex(new BN(e.upper)).toNumber(); i++) idx.add(i);
  }
  const indexes = [...idx].sort((a, b) => a - b);
  const binArrayKeys = indexes.map((i) => deriveBinArray(POOL, new BN(i), PROGRAM)[0]);
  const positionKeys = chosen.list.map((e) => new PublicKey(e.pubkey));
  const keys = [POOL, mintX, mintY, pair.reserveX, pair.reserveY, ...positionKeys, ...binArrayKeys, SYSVAR_CLOCK_PUBKEY];
  const snap = await gma(keys);
  const slot = snap.result.context.slot;
  const infos = snap.result.value.map(toInfo);
  const info = (k) => infos[keys.findIndex((x) => x.equals(k))];

  // Phase C: SDK golden from the same bytes.
  const lb = decodeAccount(program, "lbPair", info(POOL).data);
  const clock = ClockLayout.decode(info(SYSVAR_CLOCK_PUBKEY).data);
  const now = Number(clock.unixTimestamp.toString());
  const mX = unpackMint(mintX, info(mintX), info(mintX).owner);
  const mY = unpackMint(mintY, info(mintY), info(mintY).owner);
  const rX = unpackAccount(lb.reserveX, info(lb.reserveX), info(lb.reserveX).owner);
  const rY = unpackAccount(lb.reserveY, info(lb.reserveY), info(lb.reserveY).owner);
  const dx = mX.decimals;
  const dy = mY.decimals;
  const priceActive = getPriceOfBinByBinId(lb.activeId, lb.binStep).mul(new Decimal(10).pow(dx - dy));

  const fi = DLMM.calculateFeeInfo(lb.parameters.baseFactor, lb.binStep, lb.parameters.baseFeePowerFactor);
  const protocolPct = new Decimal(lb.parameters.protocolShare.toString()).mul(100).div(10000);
  const v = Object.assign({}, lb.vParameters);
  DLMM.updateReference(lb.activeId, v, lb.parameters, now);
  DLMM.updateVolatilityAccumulator(v, lb.parameters, lb.activeId);
  const totalNow = getTotalFee(lb.binStep, lb.parameters, v);
  const varNow = getVariableFee(lb.binStep, lb.parameters, v);
  const totalStored = getTotalFee(lb.binStep, lb.parameters, lb.vParameters);
  const varStored = getVariableFee(lb.binStep, lb.parameters, lb.vParameters);
  const baseRate = getBaseFee(lb.binStep, lb.parameters);

  const binArrayMap = new Map();
  const binArrays = {};
  indexes.forEach((i, n) => {
    const k = binArrayKeys[n];
    const inf = info(k);
    if (inf) {
      const ba = decodeAccount(program, "binArray", inf.data);
      binArrayMap.set(k.toBase58(), ba);
      binArrays[i] = { key: k.toBase58(), present: true };
    } else {
      binArrays[i] = { key: k.toBase58(), present: false };
    }
  });

  // Depth bands (independent JS: sum SDK-decoded bin amounts around active).
  function binAt(id) {
    const i = binIdToBinArrayIndex(new BN(id)).toNumber();
    const k = deriveBinArray(POOL, new BN(i), PROGRAM)[0].toBase58();
    const ba = binArrayMap.get(k);
    if (!ba) return null;
    const lower = i * 70;
    return ba.bins[id - lower];
  }
  const depth = [10, 25, 50].map((h) => {
    let x = new BN(0);
    let y = new BN(0);
    for (let id = lb.activeId - h; id <= lb.activeId + h; id++) {
      const b = binAt(id);
      if (!b) continue;
      x = x.add(b.amountX);
      y = y.add(b.amountY);
    }
    const base = new Decimal(x.toString()).div(new Decimal(10).pow(dx));
    const quote = new Decimal(y.toString()).div(new Decimal(10).pow(dy));
    return {
      half_width_bins: h,
      base_raw: x.toString(),
      quote_raw: y.toString(),
      base: base.toNumber(),
      quote: quote.toNumber(),
      value_quote: base.mul(priceActive).add(quote).toNumber(),
    };
  });

  const positions = [];
  for (const pk of positionKeys) {
    const w = wrapPosition(program, pk, info(pk));
    const pd = await DLMM.processPosition(program, lb, clock, w, mX, mY, null, null, binArrayMap);
    positions.push({
      position: pk.toBase58(),
      owner: w.owner().toBase58(),
      fee_owner: pd.feeOwner.toBase58(),
      lower_bin_id: pd.lowerBinId,
      upper_bin_id: pd.upperBinId,
      width: pd.upperBinId - pd.lowerBinId + 1,
      extended_bins: w.extended.length,
      data_len: info(pk).data.length,
      total_x_raw: new Decimal(pd.totalXAmount).floor().toFixed(0),
      total_y_raw: new Decimal(pd.totalYAmount).floor().toFixed(0),
      fee_x_raw: pd.feeX.toString(),
      fee_y_raw: pd.feeY.toString(),
      total_claimed_fee_x_raw: pd.totalClaimedFeeXAmount.toString(),
      total_claimed_fee_y_raw: pd.totalClaimedFeeYAmount.toString(),
      last_updated_at: pd.lastUpdatedAt.toString(),
      lower_price: getPriceOfBinByBinId(pd.lowerBinId, lb.binStep).mul(new Decimal(10).pow(dx - dy)).toNumber(),
      upper_price: getPriceOfBinByBinId(pd.upperBinId, lb.binStep).mul(new Decimal(10).pow(dx - dy)).toNumber(),
      bins_with_shares: pd.positionBinData.filter((b) => b.positionLiquidity !== "0").length,
    });
  }

  const golden = {
    sdk: "@meteora-ag/dlmm " + JSON.parse(fs.readFileSync(path.join(path.dirname(require.resolve("@meteora-ag/dlmm")), "..", "package.json"), "utf8")).version,
    slot,
    clock_unix_timestamp: now,
    pool: POOL.toBase58(),
    token_x_mint: mintX.toBase58(),
    token_y_mint: mintY.toBase58(),
    token_x_program: info(mintX).owner.toBase58(),
    token_y_program: info(mintY).owner.toBase58(),
    decimals_x: dx,
    decimals_y: dy,
    reserve_x: lb.reserveX.toBase58(),
    reserve_y: lb.reserveY.toBase58(),
    reserve_x_amount_raw: rX.amount.toString(),
    reserve_y_amount_raw: rY.amount.toString(),
    active_id: lb.activeId,
    bin_step: lb.binStep,
    status: lb.status,
    pair_type: lb.pairType,
    active_price: priceActive.toNumber(),
    active_price_str: priceActive.toString(),
    base_factor: lb.parameters.baseFactor,
    base_fee_power_factor: lb.parameters.baseFeePowerFactor,
    variable_fee_control: lb.parameters.variableFeeControl,
    protocol_share: lb.parameters.protocolShare,
    filter_period: lb.parameters.filterPeriod,
    decay_period: lb.parameters.decayPeriod,
    reduction_factor: lb.parameters.reductionFactor,
    max_volatility_accumulator: lb.parameters.maxVolatilityAccumulator,
    volatility_accumulator: lb.vParameters.volatilityAccumulator,
    volatility_reference: lb.vParameters.volatilityReference,
    index_reference: lb.vParameters.indexReference,
    v_last_update_ts: lb.vParameters.lastUpdateTimestamp.toString(),
    activation_point: lb.activationPoint.toString(),
    base_fee_pct: fi.baseFeeRatePercentage.toNumber(),
    max_fee_pct: fi.maxFeeRatePercentage.toNumber(),
    protocol_share_pct: protocolPct.toNumber(),
    base_rate: baseRate.toString(),
    stored_variable_rate: varStored.toString(),
    stored_total_rate: totalStored.toString(),
    now_volatility_accumulator: v.volatilityAccumulator,
    now_variable_rate: varNow.toString(),
    now_total_rate: totalNow.toString(),
    now_total_fee_pct: new Decimal(totalNow.toString()).div(1e9).mul(100).toNumber(),
    bin_arrays: binArrays,
    depth,
    tvl_quote_onchain: new Decimal(rX.amount.toString()).div(new Decimal(10).pow(dx)).mul(priceActive)
      .add(new Decimal(rY.amount.toString()).div(new Decimal(10).pow(dy))).toNumber(),
    owner: chosen.owner,
    extended_owner: chosen.extOwner,
    positions,
  };

  fs.mkdirSync(OUT, { recursive: true });
  fs.writeFileSync(path.join(OUT, "gma.json"), JSON.stringify(snap) + "\n");
  fs.writeFileSync(
    path.join(OUT, "meta.json"),
    JSON.stringify(
      {
        captured_at: new Date().toISOString(),
        rpc: "api.mainnet-beta.solana.com",
        slot,
        pool: POOL.toBase58(),
        owner: chosen.owner,
        extended_owner: chosen.extOwner,
        keys: keys.map((k) => k.toBase58()),
        roles: {
          pool: POOL.toBase58(),
          mint_x: mintX.toBase58(),
          mint_y: mintY.toBase58(),
          reserve_x: lb.reserveX.toBase58(),
          reserve_y: lb.reserveY.toBase58(),
          positions: positionKeys.map((k) => k.toBase58()),
          bin_array_indexes: indexes,
          bin_arrays: binArrayKeys.map((k) => k.toBase58()),
          clock: SYSVAR_CLOCK_PUBKEY.toBase58(),
        },
      },
      null,
      2
    ) + "\n"
  );
  fs.writeFileSync(path.join(OUT, "golden.json"), JSON.stringify(golden, null, 2) + "\n");
  console.error("wrote fixture at slot", slot);
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
