// Jupiter perps SOL position PDAs (seeds from delta_neutral_bot
// src/utils/jupiterPerps.ts:86-101) for the wallets used in the perps
// fixtures. Imports ONLY @solana/web3.js from the bot's node_modules.
// Usage: node scripts/golden/perps_pdas.cjs <wallet> [<wallet> ...]
const { PublicKey } = require(
  '/Users/vladimirdemidov/development/delta_neutral_bot/node_modules/@solana/web3.js',
);
const PROGRAM = new PublicKey('PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu');
const POOL = new PublicKey('5BUwFW4nRbftYTDMbgxykoFWqWHPzahFSNAaaaJtVKsq');
const SOL = new PublicKey('7xS2gz2bTp3fwCC7knJvUWTEU9Tycczu6VhJYKgi1wdz');
const USDC = new PublicKey('G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa');

function pda(wallet, side) {
  const coll = side === 'long' ? SOL : USDC;
  const [k] = PublicKey.findProgramAddressSync(
    [
      Buffer.from('position'),
      wallet.toBuffer(),
      POOL.toBuffer(),
      SOL.toBuffer(),
      coll.toBuffer(),
      Buffer.from([side === 'long' ? 1 : 2]),
    ],
    PROGRAM,
  );
  return k.toBase58();
}

for (const w of process.argv.slice(2)) {
  const k = new PublicKey(w);
  console.log(JSON.stringify({ wallet: w, long: pda(k, 'long'), short: pda(k, 'short') }));
}
