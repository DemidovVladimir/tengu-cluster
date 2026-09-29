// Compute absolute byte offsets (incl. the 8-byte Anchor discriminator) of the
// DLMM account layouts from the SDK's own IDL. Read-only; no network.
//
// Run: NODE_PATH=/Users/vladimirdemidov/development/delta_neutral_bot/node_modules \
//        node scripts/golden/dlmm_offsets.js
//
// The accounts are bytemuck `repr(C)` zero-copy structs; the program adds
// explicit padding fields so there is no implicit padding (Pod). The script
// still checks natural alignment of every scalar at its offset (BPF: u128
// aligns to 8) and reports any misalignment.
const idl = require("@meteora-ag/dlmm").IDL;

const PRIM = {
  u8: [1, 1], i8: [1, 1], bool: [1, 1],
  u16: [2, 2], i16: [2, 2],
  u32: [4, 4], i32: [4, 4], f32: [4, 4],
  u64: [8, 8], i64: [8, 8], f64: [8, 8],
  u128: [16, 8], i128: [16, 8],
  pubkey: [32, 1],
};

function typeByName(n) {
  const t = idl.types.find((t) => t.name === n);
  if (!t) throw new Error("no type " + n);
  return t;
}

function sizeOf(ty) {
  if (typeof ty === "string") {
    if (!PRIM[ty]) throw new Error("prim " + ty);
    return PRIM[ty][0];
  }
  if (ty.array) return sizeOf(ty.array[0]) * ty.array[1];
  if (ty.defined) {
    const t = typeByName(ty.defined.name);
    if (t.type.kind === "struct") {
      return t.type.fields.reduce((a, f) => a + sizeOf(f.type), 0);
    }
    if (t.type.kind === "enum") return 1;
  }
  throw new Error("size " + JSON.stringify(ty));
}

// Flatten a struct into [path, offset, type] leaves.
function walk(ty, base, path, out, depth) {
  if (typeof ty === "string") {
    out.push([path, base, ty]);
    return;
  }
  if (ty.array) {
    const [el, n] = ty.array;
    const es = sizeOf(el);
    if (typeof el === "string" || depth > 1) {
      out.push([path, base, `[${typeof el === "string" ? el : JSON.stringify(el)};${n}] (${es}B each)`]);
      return;
    }
    // expand only element 0 of struct arrays
    out.push([path, base, `[${el.defined.name};${n}] (${es}B each)`]);
    walk(el, base, path + "[0]", out, depth + 1);
    return;
  }
  if (ty.defined) {
    const t = typeByName(ty.defined.name);
    if (t.type.kind === "enum") {
      out.push([path, base, "enum " + ty.defined.name]);
      return;
    }
    let off = base;
    for (const f of t.type.fields) {
      const s = sizeOf(f.type);
      if (typeof f.type === "string") {
        const al = PRIM[f.type][1];
        if (off % al !== 0) out.push([path + "." + f.name, off, f.type + "  !!MISALIGNED"]);
      }
      walk(f.type, off, path ? path + "." + f.name : f.name, out, depth);
      off += s;
    }
    return;
  }
  throw new Error("walk " + JSON.stringify(ty));
}

const names = process.argv.slice(2).length
  ? process.argv.slice(2)
  : ["LbPair", "PositionV2", "BinArray", "Bin", "FeeInfo"];
for (const n of names) {
  const acct = idl.accounts.find((a) => a.name === n);
  const disc = acct ? 8 : 0;
  const size = sizeOf({ defined: { name: n } });
  console.log(`== ${n} disc=${acct ? JSON.stringify(acct.discriminator) : "-"} size=${size} total=${size + disc}`);
  const out = [];
  walk({ defined: { name: n } }, disc, "", out, 0);
  for (const [p, o, t] of out) console.log(`${String(o).padStart(6)}  ${p}  ${t}`);
}
