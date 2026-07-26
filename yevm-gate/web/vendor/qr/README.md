# Vendored QR encoder

`qr.js` is [`qrcode-generator`](https://github.com/kazuhikoarase/qrcode-generator)
**v1.4.4** (MIT, Kazuhiko Arase) — a single self-contained UMD file, no deps, no
network. Loaded as a plain `<script src="/static/qr.js">` (defines the global
`qrcode`), embedded in the binary via `include_bytes!` and served same-origin.

Used to render the WalletConnect pairing QR ourselves (`showQrModal: false` +
the `display_uri` event), so the connect flow has **no dependency on
`api.web3modal.org`** — which content blockers routinely kill, breaking WC's own
modal. Vendored on purpose; there is no JS build step.

Regenerate: `curl -fsSL https://unpkg.com/qrcode-generator@1.4.4/qrcode.js -o qr.js`
