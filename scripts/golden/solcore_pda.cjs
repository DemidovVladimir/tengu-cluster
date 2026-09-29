// Golden PDA / ATA vectors for WP-SOLCORE, computed with @solana/web3.js from
// the delta_neutral_bot node_modules (read-only; never imports bot src/config).
// Usage: node scripts/golden/solcore_pda.cjs
// Pinned into `src/domain/solana.rs` tests (`pda_*`).
const path = require('path');
const { createRequire } = require('module');

const BOT = process.env.BOT_DIR || '/Users/vladimirdemidov/development/delta_neutral_bot';
const req = createRequire(path.join(BOT, 'package.json'));
const { PublicKey } = req('@solana/web3.js');

const PERPS = new PublicKey('PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu');
const JLP_POOL = new PublicKey('5BUwFW4nRbftYTDMbgxykoFWqWHPzahFSNAaaaJtVKsq');
const CUST_SOL = new PublicKey('7xS2gz2bTp3fwCC7knJvUWTEU9Tycczu6VhJYKgi1wdz');
const CUST_USDC = new PublicKey('G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa');
const TOKEN = new PublicKey('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA');
const ATA = new PublicKey('ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL');
const DLMM = new PublicKey('LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo');
const wallet = new PublicKey('F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S');
const pool = new PublicKey('5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6');

function jup(side) {
  const coll = side === 'long' ? CUST_SOL : CUST_USDC;
  const b = side === 'long' ? 1 : 2;
  const [pda, bump] = PublicKey.findProgramAddressSync(
    [
      Buffer.from('position'),
      wallet.toBuffer(),
      JLP_POOL.toBuffer(),
      CUST_SOL.toBuffer(),
      coll.toBuffer(),
      Buffer.from([b]),
    ],
    PERPS,
  );
  return { pda: pda.toBase58(), bump };
}

function ata(owner, mint) {
  const [a, bump] = PublicKey.findProgramAddressSync(
    [owner.toBuffer(), TOKEN.toBuffer(), mint.toBuffer()],
    ATA,
  );
  return { ata: a.toBase58(), bump };
}

function binArray(index) {
  const idx = Buffer.alloc(8);
  idx.writeBigInt64LE(BigInt(index));
  const [a, bump] = PublicKey.findProgramAddressSync(
    [Buffer.from('bin_array'), pool.toBuffer(), idx],
    DLMM,
  );
  return { index, pda: a.toBase58(), bump };
}

function single(seed, program) {
  const [p, b] = PublicKey.findProgramAddressSync([Buffer.from(seed)], program);
  return { pda: p.toBase58(), bump: b };
}

const out = {
  jup_long: jup('long'),
  jup_short: jup('short'),
  ata_usdc: ata(wallet, new PublicKey('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v')),
  ata_98s: ata(wallet, new PublicKey('98sMhvDwXj1RQi5c5Mndm3vPe9cBqPrbLaufMXFNMh5g')),
  ata_wsol: ata(wallet, new PublicKey('So11111111111111111111111111111111111111112')),
  bin_arrays: [-79, -78, -77, -76, -75, 0, 1, -1].map(binArray),
  perpetuals: single('perpetuals', PERPS),
  event_authority: single('__event_authority', PERPS),
  on_curve: {},
};
out.on_curve.wallet = PublicKey.isOnCurve(wallet.toBytes());
out.on_curve.jup_short = PublicKey.isOnCurve(new PublicKey(out.jup_short.pda).toBytes());
console.log(JSON.stringify(out, null, 2));
