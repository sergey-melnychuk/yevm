# Vendored WalletConnect bundle

`wc.js` is a **self-contained ESM bundle** of
[`@walletconnect/ethereum-provider`](https://www.npmjs.com/package/@walletconnect/ethereum-provider)
with all Node built-ins (Buffer / crypto / process / events / stream) polyfilled
in. The gate embeds it via `include_bytes!` and serves it same-origin at
`/static/wc.js`, so there is **no third-party CDN at runtime**. The web
UI imports it lazily (only when the user clicks *WalletConnect (mobile)*).

It is used for exactly two actions, both with keys that live in a mobile wallet:
- **SIWE** sign-in (`personal_sign`)
- signing a **Burn** tx (`eth_sendTransaction`)

This file is a build artifact, committed on purpose. `cargo build` does **not**
run any JS toolchain — regenerating the bundle is a manual vendoring step, like
`wasm-pack`.

## Pinned versions

| package | version |
|---|---|
| `@walletconnect/ethereum-provider` | `2.17.0` |
| `esbuild` | `0.28.1` |
| `esbuild-plugin-polyfill-node` | `0.3.0` |

Bump deliberately (supply-chain surface) and re-verify the output is
self-contained (see *Verify* below).

## Regenerate

In a scratch directory (nothing here needs to be committed except the output):

```sh
npm i @walletconnect/ethereum-provider@2.17.0 esbuild@0.28.1 esbuild-plugin-polyfill-node@0.3.0
```

`entry.mjs`:

```js
export { EthereumProvider } from '@walletconnect/ethereum-provider';
```

`build.mjs`:

```js
import { build } from 'esbuild';
import { polyfillNode } from 'esbuild-plugin-polyfill-node';

await build({
  entryPoints: ['entry.mjs'],
  outfile: 'wc.js',
  bundle: true,
  format: 'esm',
  platform: 'browser',
  target: 'es2022',
  minify: true,
  define: { global: 'globalThis', 'process.env.NODE_ENV': '"production"' },
  plugins: [polyfillNode({ polyfills: { crypto: true, buffer: true, process: true, events: true, stream: true } })],
});
```

Then build and drop the result here:

```sh
node build.mjs
cp wc.js /path/to/yevm/yevm-gate/web/vendor/wc/wc.js
```

## Verify the bundle is self-contained

These must all print nothing (no external/static imports, no remote `import()`)
and the export must be present:

```sh
grep -oE '^import[^;]+from[ ]*"[^"]+"' wc.js          # → (empty)
grep -oE 'import\("https?://[^"]+' wc.js              # → (empty)
grep -oE 'from[ ]*"/node/[^"]+"' wc.js               # → (empty)
grep -oE 'export\{[^}]*EthereumProvider[^}]*\}' wc.js # → export{... as EthereumProvider}
python3 -c "print('NUL:', open('wc.js','rb').read().count(0))"  # → NUL: 0
```

## Configure

Set the **`YEVM_WALLETCONNECT_PROJECT_ID`** env var to a WalletConnect Cloud
(<https://cloud.reown.com>) project id when running the gate:

```sh
YEVM_WALLETCONNECT_PROJECT_ID=<your-project-id> cargo run -p yevm-gate
```

The server reads it at request time (like every other `YEVM_*` var) and injects
it into the UI's `walletconnect-project-id` meta tag, so the id is **never
committed** to `index.html`. Without it the *WalletConnect (mobile)* menu entry
errors out gracefully.
