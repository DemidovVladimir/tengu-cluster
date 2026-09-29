// Goldens for the phase-6b write path: ed25519 signatures, legacy / v0
// message bytes, and every instruction the write tools build (System, SPL
// Token, ATA, ComputeBudget, Meteora DLMM, Jupiter perps) — produced by the
// SAME libraries delta_neutral_bot sends with (web3.js, spl-token, the
// Meteora SDK's own Anchor program, anchor 0.29 + the perps IDL). Offline:
// no RPC call is made (the Connection is never used).
//
// Run: node scripts/golden/write_ixs.cjs
// Writes tests/fixtures/solana/tx/golden.json.
//
// Keys are deterministic: signers from Keypair.fromSeed([n; 32]), the rest
// from k(n) = 32 bytes (i*7 + n) % 256 — the Rust test rebuilds both.
const BOT = '/Users/vladimirdemidov/development/delta_neutral_bot';
const req = (m) => require(`${BOT}/node_modules/${m}`);
const fs = require('fs');
const path = require('path');

const web3 = req('@solana/web3.js');
const spl = req('@solana/spl-token');
const dlmm = req('@meteora-ag/dlmm');
const jupAnchor = req('jup-anchor');
const perpsIdl = require(`${BOT}/src/idl/jupiter-perps-idl.json`);
const { BN } = jupAnchor;
const {
  PublicKey,
  Keypair,
  SystemProgram,
  ComputeBudgetProgram,
  TransactionMessage,
  VersionedTransaction,
  AddressLookupTableAccount,
} = web3;

const hex = (b) => Buffer.from(b).toString('hex');
const k = (n) => new PublicKey(Uint8Array.from({ length: 32 }, (_, i) => (i * 7 + n) % 256));
const seedKp = (n) => Keypair.fromSeed(new Uint8Array(32).fill(n));
const ixJson = (name, ix) => ({
  name,
  program: ix.programId.toBase58(),
  data: hex(ix.data),
  metas: ix.keys.map((m) => [m.pubkey.toBase58(), m.isWritable, m.isSigner]),
});

const WSOL = new PublicKey('So11111111111111111111111111111111111111112');
const USDC = new PublicKey('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
const RENT = new PublicKey('SysvarRent111111111111111111111111111111111');
const JUP_PERPS = new PublicKey('PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu');
const JLP_POOL = new PublicKey('5BUwFW4nRbftYTDMbgxykoFWqWHPzahFSNAaaaJtVKsq');
const CUSTODY_SOL = new PublicKey('7xS2gz2bTp3fwCC7knJvUWTEU9Tycczu6VhJYKgi1wdz');
const CUSTODY_USDC = new PublicKey('G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa');

async function main() {
  const wallet = seedKp(1);
  const positionKp = seedKp(2);
  const out = { signers: {}, ed25519: [], ixs: [], pdas: {}, messages: [] };
  out.signers.wallet = { seed: hex(new Uint8Array(32).fill(1)), pubkey: wallet.publicKey.toBase58() };
  out.signers.position = { seed: hex(new Uint8Array(32).fill(2)), pubkey: positionKp.publicKey.toBase58() };

  // ── ed25519 (web3.js → @noble/curves) ─────────────────────────────────
  for (const msg of [Buffer.alloc(0), Buffer.from('tengu'), Buffer.alloc(300, 0xab)]) {
    const sig = web3.Keypair.fromSeed(new Uint8Array(32).fill(1));
    const { ed25519 } = req('@noble/curves/ed25519');
    out.ed25519.push({ msg: hex(msg), sig: hex(ed25519.sign(msg, sig.secretKey.slice(0, 32))) });
  }

  // ── System / SPL / ATA / ComputeBudget ────────────────────────────────
  const ixs = out.ixs;
  ixs.push(ixJson('system_transfer', SystemProgram.transfer({ fromPubkey: wallet.publicKey, toPubkey: k(10), lamports: 123456789 })));
  ixs.push(ixJson('sync_native', spl.createSyncNativeInstruction(k(11))));
  ixs.push(ixJson('close_account', spl.createCloseAccountInstruction(k(12), wallet.publicKey, wallet.publicKey, [], spl.TOKEN_PROGRAM_ID)));
  ixs.push(ixJson('close_account_2022', spl.createCloseAccountInstruction(k(13), wallet.publicKey, wallet.publicKey, [], spl.TOKEN_2022_PROGRAM_ID)));
  const wsolAta = spl.getAssociatedTokenAddressSync(WSOL, wallet.publicKey, false, spl.TOKEN_PROGRAM_ID);
  ixs.push(ixJson('ata_idempotent', spl.createAssociatedTokenAccountIdempotentInstruction(wallet.publicKey, wsolAta, wallet.publicKey, WSOL, spl.TOKEN_PROGRAM_ID)));
  ixs.push(ixJson('cu_limit', ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 })));
  ixs.push(ixJson('cu_price', ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 12345 })));

  // ── Meteora DLMM (the SDK's own Anchor program, lb_clmm IDL) ──────────
  const program = dlmm.createProgram(new web3.Connection('http://127.0.0.1:1'));
  const lbPair = k(20);
  const zeroSlices = { slices: [{ accountsType: { transferHookX: {} }, length: 0 }, { accountsType: { transferHookY: {} }, length: 0 }] };
  const binArrays = [-1, 0].map((i) => ({ pubkey: dlmm.deriveBinArray(lbPair, new BN(i), program.programId)[0], isSigner: false, isWritable: true }));
  out.pdas.bin_array_lb20_m1 = binArrays[0].pubkey.toBase58();
  out.pdas.bin_array_lb20_0 = binArrays[1].pubkey.toBase58();
  out.pdas.bitmap_ext_lb20 = dlmm.deriveBinArrayBitmapExtension(lbPair, program.programId)[0].toBase58();

  ixs.push(ixJson('dlmm_initialize_position', await program.methods
    .initializePosition(-39, 70)
    .accountsPartial({ payer: wallet.publicKey, position: positionKp.publicKey, lbPair, owner: wallet.publicKey, rent: RENT })
    .instruction()));
  ixs.push(ixJson('dlmm_initialize_bin_array', await program.methods
    .initializeBinArray(new BN(-1))
    .accountsPartial({ lbPair, binArray: binArrays[0].pubkey, funder: wallet.publicKey })
    .instruction()));

  const addAccounts = (ext) => ({
    position: positionKp.publicKey, lbPair, binArrayBitmapExtension: ext,
    userTokenX: k(21), userTokenY: k(22), reserveX: k(23), reserveY: k(24),
    tokenXMint: WSOL, tokenYMint: USDC, sender: wallet.publicKey,
    tokenXProgram: spl.TOKEN_PROGRAM_ID, tokenYProgram: spl.TOKEN_PROGRAM_ID,
  });
  const liq = {
    amountX: new BN('1500000000'), amountY: new BN('250000000'), activeId: -5, maxActiveBinSlippage: 1,
    strategyParameters: { minBinId: -39, maxBinId: 30, strategyType: { spotImBalanced: {} }, parameteres: new Array(64).fill(0) },
  };
  for (const [name, ext] of [['dlmm_add_liquidity_by_strategy2', null], ['dlmm_add_liquidity_by_strategy2_ext', k(25)]]) {
    ixs.push(ixJson(name, await program.methods
      .addLiquidityByStrategy2(liq, zeroSlices)
      .accountsPartial(addAccounts(ext))
      .remainingAccounts(binArrays)
      .instruction()));
  }
  const bidask = { ...liq, strategyParameters: { ...liq.strategyParameters, strategyType: { bidAskImBalanced: {} } } };
  ixs.push(ixJson('dlmm_add_liquidity_bidask', await program.methods
    .addLiquidityByStrategy2(bidask, zeroSlices).accountsPartial(addAccounts(null)).remainingAccounts(binArrays).instruction()));

  ixs.push(ixJson('dlmm_remove_liquidity_by_range2', await program.methods
    .removeLiquidityByRange2(-39, 30, 10000, zeroSlices)
    .accountsPartial({ ...addAccounts(null), memoProgram: dlmm.MEMO_PROGRAM_ID })
    .remainingAccounts(binArrays)
    .instruction()));
  ixs.push(ixJson('dlmm_claim_fee2', await program.methods
    .claimFee2(-39, 30, zeroSlices)
    .accountsPartial({
      lbPair, position: positionKp.publicKey, sender: wallet.publicKey, reserveX: k(23), reserveY: k(24),
      userTokenX: k(21), userTokenY: k(22), tokenXMint: WSOL, tokenYMint: USDC,
      tokenProgramX: spl.TOKEN_PROGRAM_ID, tokenProgramY: spl.TOKEN_PROGRAM_ID, memoProgram: dlmm.MEMO_PROGRAM_ID,
    })
    .remainingAccounts(binArrays)
    .instruction()));
  ixs.push(ixJson('dlmm_claim_reward2', await program.methods
    .claimReward2(new BN(1), -39, 30, { slices: [{ accountsType: { transferHookReward: {} }, length: 0 }] })
    .accountsPartial({
      lbPair, position: positionKp.publicKey, sender: wallet.publicKey, rewardVault: k(26), rewardMint: k(27),
      userTokenAccount: k(28), tokenProgram: spl.TOKEN_PROGRAM_ID, memoProgram: dlmm.MEMO_PROGRAM_ID,
    })
    .remainingAccounts(binArrays)
    .instruction()));
  ixs.push(ixJson('dlmm_close_position_if_empty', await program.methods
    .closePositionIfEmpty()
    .accountsPartial({ position: positionKp.publicKey, sender: wallet.publicKey, rentReceiver: wallet.publicKey })
    .instruction()));

  // ── Jupiter perps (anchor 0.29 + the bot's IDL) ───────────────────────
  const provider = new jupAnchor.AnchorProvider(new web3.Connection('http://127.0.0.1:1'), new jupAnchor.Wallet(wallet), {});
  const perps = new jupAnchor.Program(perpsIdl, JUP_PERPS, provider);
  const pda = (seeds) => PublicKey.findProgramAddressSync(seeds, JUP_PERPS)[0];
  const perpetuals = pda([Buffer.from('perpetuals')]);
  const eventAuthority = pda([Buffer.from('__event_authority')]);
  const position = k(30);
  const counter = new BN('987654321');
  const requestPda = (change) => pda([Buffer.from('position_request'), position.toBuffer(), counter.toArrayLike(Buffer, 'le', 8), Buffer.from([change])]);
  out.pdas.perpetuals = perpetuals.toBase58();
  out.pdas.perps_event_authority = eventAuthority.toBase58();
  out.pdas.request_increase = requestPda(1).toBase58();
  out.pdas.request_decrease = requestPda(2).toBase58();
  out.pdas.dlmm_event_authority = PublicKey.findProgramAddressSync([Buffer.from('__event_authority')], program.programId)[0].toBase58();
  const common = (request, mint, collateralCustody) => ({
    owner: wallet.publicKey, perpetuals, pool: JLP_POOL, position, positionRequest: request,
    positionRequestAta: spl.getAssociatedTokenAddressSync(mint, request, true, spl.TOKEN_PROGRAM_ID),
    custody: CUSTODY_SOL, collateralCustody, referral: null,
    tokenProgram: spl.TOKEN_PROGRAM_ID, associatedTokenProgram: spl.ASSOCIATED_TOKEN_PROGRAM_ID,
    systemProgram: SystemProgram.programId, eventAuthority, program: JUP_PERPS,
  });
  const usdcAta = spl.getAssociatedTokenAddressSync(USDC, wallet.publicKey, false, spl.TOKEN_PROGRAM_ID);
  ixs.push(ixJson('perps_increase_short', await perps.methods
    .createIncreasePositionMarketRequest({
      sizeUsdDelta: new BN('1000000000'), collateralTokenDelta: new BN('250000000'), side: { short: {} },
      priceSlippage: new BN('150750000'), jupiterMinimumOut: null, counter,
    })
    .accounts({ ...common(requestPda(1), USDC, CUSTODY_USDC), fundingAccount: usdcAta, inputMint: USDC })
    .instruction()));
  ixs.push(ixJson('perps_increase_long_minout', await perps.methods
    .createIncreasePositionMarketRequest({
      sizeUsdDelta: new BN('1000000000'), collateralTokenDelta: new BN('1500000000'), side: { long: {} },
      priceSlippage: new BN('152250000'), jupiterMinimumOut: new BN(42), counter,
    })
    .accounts({ ...common(requestPda(1), WSOL, CUSTODY_SOL), fundingAccount: wsolAta, inputMint: WSOL })
    .instruction()));
  ixs.push(ixJson('perps_decrease_short_entire', await perps.methods
    .createDecreasePositionMarketRequest({
      collateralUsdDelta: new BN(0), sizeUsdDelta: new BN(0), priceSlippage: new BN('151500000'),
      jupiterMinimumOut: null, entirePosition: true, counter,
    })
    .accounts({ ...common(requestPda(2), USDC, CUSTODY_USDC), receivingAccount: usdcAta, desiredMint: USDC })
    .instruction()));
  ixs.push(ixJson('perps_decrease_long_partial', await perps.methods
    .createDecreasePositionMarketRequest({
      collateralUsdDelta: new BN('5000000'), sizeUsdDelta: new BN('20000000'), priceSlippage: new BN('149250000'),
      jupiterMinimumOut: null, entirePosition: null, counter,
    })
    .accounts({ ...common(requestPda(2), WSOL, CUSTODY_SOL), receivingAccount: wsolAta, desiredMint: WSOL })
    .instruction()));

  // ── Messages: legacy compile + sign, v0 parse ────────────────────────
  const blockhash = k(40).toBase58();
  const byName = (n) => {
    const j = ixs.find((x) => x.name === n);
    return new web3.TransactionInstruction({
      programId: new PublicKey(j.program),
      data: Buffer.from(j.data, 'hex'),
      keys: j.metas.map(([p, w, s]) => ({ pubkey: new PublicKey(p), isWritable: w, isSigner: s })),
    });
  };
  const legacyIxs = ['cu_limit', 'cu_price', 'dlmm_initialize_position', 'ata_idempotent', 'system_transfer', 'sync_native', 'dlmm_add_liquidity_by_strategy2', 'close_account'];
  const legacy = new TransactionMessage({ payerKey: wallet.publicKey, recentBlockhash: blockhash, instructions: legacyIxs.map(byName) }).compileToLegacyMessage();
  const legacyTx = new VersionedTransaction(legacy);
  legacyTx.sign([wallet, positionKp]);
  out.messages.push({
    name: 'legacy_open', payer: wallet.publicKey.toBase58(), blockhash, ixs: legacyIxs,
    message: hex(legacy.serialize()), tx: hex(legacyTx.serialize()),
  });

  // v0 with a lookup table and a foreign fee payer (Ultra gasless shape):
  // our wallet is the SECOND required signer.
  const payer = seedKp(3);
  const alt = new AddressLookupTableAccount({ key: k(50), state: { deactivationSlot: BigInt('18446744073709551615'), lastExtendedSlot: 0, lastExtendedSlotStartIndex: 0, authority: undefined, addresses: [k(10), k(11), k(21), k(22)] } });
  const v0Ixs = ['system_transfer', 'sync_native', 'close_account'];
  const v0 = new TransactionMessage({ payerKey: payer.publicKey, recentBlockhash: blockhash, instructions: v0Ixs.map(byName) }).compileToV0Message([alt]);
  const v0Unsigned = new VersionedTransaction(v0);
  const v0Signed = new VersionedTransaction(v0);
  v0Signed.sign([wallet]);
  out.messages.push({
    name: 'v0_foreign_payer', payer: payer.publicKey.toBase58(), blockhash, ixs: v0Ixs,
    message: hex(v0.serialize()), unsigned_tx: hex(v0Unsigned.serialize()), wallet_signed_tx: hex(v0Signed.serialize()),
    num_required_signatures: v0.header.numRequiredSignatures,
    static_keys: v0.staticAccountKeys.map((p) => p.toBase58()),
  });

  const dir = path.join(__dirname, '..', '..', 'tests', 'fixtures', 'solana', 'tx');
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'golden.json'), JSON.stringify(out, null, 1) + '\n');
  console.log(`wrote ${ixs.length} ixs, ${out.messages.length} messages, ${out.ed25519.length} signatures`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
