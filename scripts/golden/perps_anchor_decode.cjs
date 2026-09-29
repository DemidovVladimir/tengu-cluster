// Cross-check scripts/golden/perps_decode.py against Anchor 0.29's own
// BorshAccountsCoder (the decoder delta_neutral_bot uses live). Imports ONLY
// from the bot's node_modules (`jup-anchor` = @coral-xyz/anchor@0.29) and
// reads its IDL JSON; never the bot's src/config.
// Usage: node scripts/golden/perps_anchor_decode.cjs tests/fixtures/solana/perps/gma.json
const BOT = '/Users/vladimirdemidov/development/delta_neutral_bot';
const anchor = require(`${BOT}/node_modules/jup-anchor`);
const idl = require(`${BOT}/src/idl/jupiter-perps-idl.json`);
const fs = require('fs');

const coder = new anchor.BorshAccountsCoder(idl);
const gma = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const names = {
  '01b830515d833f91': 'Custody',
  aabc8fe47a40f7d0: 'Position',
  f19a6d0411b16dbc: 'Pool',
  '0c26fac72e9a20d8': 'PositionRequest',
};

const show = (v) => {
  if (v && typeof v.toBase58 === 'function') return v.toBase58();
  if (anchor.BN.isBN(v)) return v.toString();
  if (Array.isArray(v)) return v.map(show);
  if (v && typeof v === 'object') return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, show(x)]));
  return v;
};

console.log('slot', gma.result.context.slot);
gma.result.value.forEach((v, i) => {
  if (!v) return console.log(i, 'absent');
  const buf = Buffer.from(v.data[0], 'base64');
  const name = names[buf.subarray(0, 8).toString('hex')];
  const acc = coder.decode(name, buf);
  const s = show(acc);
  if (name === 'Custody') {
    delete s.priceImpactBuffer;
  }
  console.log(i, name, JSON.stringify(s));
});
