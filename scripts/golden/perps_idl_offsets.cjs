// Compute absolute byte offsets of Jupiter perps accounts from the Anchor IDL
// (delta_neutral_bot/src/idl/jupiter-perps-idl.json, read-only) — used to
// verify the offsets hard-coded in src/domain/lp/perps.rs.
// Usage: node scripts/golden/perps_idl_offsets.cjs [path-to-idl]
const path =
  process.argv[2] ||
  '/Users/vladimirdemidov/development/delta_neutral_bot/src/idl/jupiter-perps-idl.json';
const crypto = require('crypto');
const idl = require(path);
const types = Object.fromEntries(idl.types.map((t) => [t.name, t.type]));
const PRIM = {
  u8: 1, i8: 1, bool: 1, u16: 2, i16: 2, u32: 4, i32: 4, f32: 4,
  u64: 8, i64: 8, f64: 8, u128: 16, i128: 16, publicKey: 32,
};

function size(t) {
  if (typeof t === 'string') {
    if (t in PRIM) return PRIM[t];
    throw new Error('dynamic ' + t);
  }
  if (t.defined) {
    const d = types[t.defined];
    if (d.kind === 'struct') return d.fields.reduce((s, f) => s + size(f.type), 0);
    if (d.kind === 'enum') {
      if (d.variants.every((v) => !v.fields)) return 1;
      throw new Error('data enum ' + t.defined);
    }
  }
  if (t.array) return size(t.array[0]) * t.array[1];
  throw new Error('dynamic ' + JSON.stringify(t));
}

function walk(fields, base, prefix, out) {
  let off = base;
  for (const f of fields) {
    const t = f.type;
    const name = prefix + f.name;
    if (t.defined && types[t.defined].kind === 'struct') {
      out.push([name, off, t.defined, size(t)]);
      walk(types[t.defined].fields, off, name + '.', out);
    } else {
      let s;
      try {
        s = size(t);
      } catch (e) {
        out.push([name, off, JSON.stringify(t), 'dynamic']);
        return out;
      }
      out.push([name, off, typeof t === 'string' ? t : JSON.stringify(t), s]);
    }
    try {
      off += size(t);
    } catch (e) {
      return out;
    }
  }
  out.push(['<end>', off, '', '']);
  return out;
}

for (const name of ['Position', 'Custody', 'Pool', 'PositionRequest']) {
  const acc = idl.accounts.find((a) => a.name === name);
  const disc = crypto.createHash('sha256').update('account:' + name).digest().subarray(0, 8);
  console.log(`\n== ${name} disc=${disc.toString('hex')} [${[...disc].join(',')}]`);
  for (const [n, off, ty, s] of walk(acc.type.fields, 8, '', [])) {
    if (!n.includes('.') || process.env.NESTED !== '0') {
      console.log(`${String(off).padStart(5)}  ${n}  ${ty}  ${s}`);
    }
  }
}
