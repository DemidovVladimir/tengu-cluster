// Golden borrow APR / accrued fee / liquidation price computed with the
// bot's OWN math (delta_neutral_bot/src/utils/jupiterPerps.ts:55-64, 204-318,
// copied verbatim minus TS types) over accounts decoded by Anchor 0.29's
// BorshAccountsCoder, with BN. The bot module itself is not imported (it
// loads src/config). Imports ONLY from the bot's node_modules.
// Usage: node scripts/golden/perps_ts_math.cjs tests/fixtures/solana/perps/gma.json
const BOT = '/Users/vladimirdemidov/development/delta_neutral_bot';
const anchor = require(`${BOT}/node_modules/jup-anchor`);
const idl = require(`${BOT}/src/idl/jupiter-perps-idl.json`);
const fs = require('fs');

const BN = anchor.BN;
// ---- verbatim from jupiterPerps.ts ----------------------------------------
const USD_PRECISION = 1_000_000;
const RATE_POWER = new BN(1_000_000_000);
const DEBT_POWER = RATE_POWER;
const BPS_POWER = new BN(10_000);
const HOURS_IN_A_YEAR = 24 * 365;

function divCeil(a, b) {
  const q = a.div(b);
  return a.mod(b).isZero() ? q : q.addn(1);
}

function getDebt(custody) {
  return divCeil(BN.max(custody.debt.sub(custody.borrowLendInterestsAccured), new BN(0)), DEBT_POWER);
}

function hourlyBorrowRate(custody) {
  const debt = getDebt(custody);
  const owned = custody.assets.owned.add(debt);
  const locked = custody.assets.locked.add(debt);
  if (!(owned.gtn(0) && locked.gtn(0))) return new BN(0);

  const util = locked.mul(RATE_POWER).div(owned);
  const { minRateBps, maxRateBps, targetRateBps, targetUtilizationRate } = custody.jumpRateState;

  let yearlyRate;
  if (util.lte(targetUtilizationRate)) {
    yearlyRate = divCeil(targetRateBps.sub(minRateBps).mul(util), targetUtilizationRate)
      .add(minRateBps)
      .mul(RATE_POWER)
      .div(BPS_POWER);
  } else {
    const rateDiff = BN.max(new BN(0), maxRateBps.sub(targetRateBps));
    const utilDiff = BN.max(new BN(0), util.sub(targetUtilizationRate));
    const denom = BN.max(new BN(0), RATE_POWER.sub(targetUtilizationRate));
    if (denom.isZero()) throw new Error('jupiterPerps: borrow-rate denominator is 0');
    yearlyRate = divCeil(rateDiff.mul(utilDiff), denom).add(targetRateBps).mul(RATE_POWER).div(BPS_POWER);
  }
  return yearlyRate.divn(HOURS_IN_A_YEAR);
}

function borrowAprPct(custody) {
  return (hourlyBorrowRate(custody).toNumber() / RATE_POWER.toNumber()) * HOURS_IN_A_YEAR * 100;
}

function accruedBorrowFeeUsdBn(position, collateralCustody) {
  return collateralCustody.fundingRateState.cumulativeInterestRate
    .sub(position.cumulativeInterestSnapshot)
    .mul(position.sizeUsd)
    .div(RATE_POWER);
}

function computeLiquidationPrice(position, custody, collateralCustody) {
  if (!position || position.sizeUsd.isZero()) return null;
  const maxLeverage = custody?.pricing?.maxLeverage;
  if (!maxLeverage || maxLeverage.isZero()) return null;

  const scalar = custody?.pricing?.tradeImpactFeeScalar;
  const priceImpactFeeBps =
    scalar && !scalar.isZero() ? divCeil(position.sizeUsd.mul(BPS_POWER), scalar) : new BN(0);
  const totalFeeBps = custody.decreasePositionBps.add(priceImpactFeeBps);
  const closeFeeUsd = position.sizeUsd.mul(totalFeeBps).div(BPS_POWER);

  const borrowFeeUsd = accruedBorrowFeeUsdBn(position, collateralCustody);

  const totalFeeUsd = closeFeeUsd.add(borrowFeeUsd);
  const maxLossUsd = position.sizeUsd.mul(BPS_POWER).div(maxLeverage).add(totalFeeUsd);
  const marginUsd = position.collateralUsd;

  const maxPriceDiff = maxLossUsd.sub(marginUsd).abs().mul(position.price).div(position.sizeUsd);

  const isLong = !!position.side.long;
  const underMargined = maxLossUsd.gt(marginUsd);
  let liqBn;
  if (isLong) {
    liqBn = underMargined ? position.price.add(maxPriceDiff) : position.price.sub(maxPriceDiff);
  } else {
    liqBn = underMargined ? position.price.sub(maxPriceDiff) : position.price.add(maxPriceDiff);
  }

  const liq = liqBn.toNumber() / USD_PRECISION;
  return liq > 0 ? liq : 0;
}
// ---- end verbatim ---------------------------------------------------------

const coder = new anchor.BorshAccountsCoder(idl);
const gma = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const vals = gma.result.value;
const dec = (name, v) => coder.decode(name, Buffer.from(v.data[0], 'base64'));
const sol = dec('Custody', vals[0]);
const usdc = dec('Custody', vals[1]);
const util = (c) => {
  const debt = getDebt(c);
  return c.assets.locked.add(debt).mul(RATE_POWER).div(c.assets.owned.add(debt)).toString();
};
console.log('slot', gma.result.context.slot);
console.log('sol', { util_raw: util(sol), hourly: hourlyBorrowRate(sol).toString(), apr_pct: borrowAprPct(sol) });
console.log('usdc', { debt: getDebt(usdc).toString(), util_raw: util(usdc), hourly: hourlyBorrowRate(usdc).toString(), apr_pct: borrowAprPct(usdc) });
vals.slice(3).forEach((v, i) => {
  if (!v) return;
  const p = dec('Position', v);
  const coll = p.side.long ? sol : usdc;
  console.log(i + 3, p.owner.toBase58(), p.side.long ? 'long' : 'short', {
    size_usd: p.sizeUsd.toString(),
    accrued_fee_raw: accruedBorrowFeeUsdBn(p, coll).toString(),
    liq: computeLiquidationPrice(p, sol, coll),
  });
});
