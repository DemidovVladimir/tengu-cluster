// Golden ATA derivation for tests/fixtures/solana/wallet (independent of the
// Rust code). Imports ONLY from delta_neutral_bot/node_modules; no secrets,
// no network. Run: node scripts/golden/wallet_atas.cjs
const path = require('path');
const nm = '/Users/vladimirdemidov/development/delta_neutral_bot/node_modules';
const { PublicKey } = require(path.join(nm, '@solana/web3.js'));
const {
  getAssociatedTokenAddressSync,
  TOKEN_PROGRAM_ID,
  TOKEN_2022_PROGRAM_ID,
} = require(path.join(nm, '@solana/spl-token'));

const wallet = new PublicKey('F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S');
const rows = [
  ['So11111111111111111111111111111111111111112', TOKEN_PROGRAM_ID],
  ['EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v', TOKEN_PROGRAM_ID],
  ['98sMhvDwXj1RQi5c5Mndm3vPe9cBqPrbLaufMXFNMh5g', TOKEN_PROGRAM_ID],
  ['2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo', TOKEN_2022_PROGRAM_ID],
];
for (const [mint, prog] of rows) {
  const ata = getAssociatedTokenAddressSync(new PublicKey(mint), wallet, false, prog);
  console.log(
    JSON.stringify({
      wallet: wallet.toBase58(),
      mint,
      token_program: prog.toBase58(),
      ata: ata.toBase58(),
    }),
  );
}
