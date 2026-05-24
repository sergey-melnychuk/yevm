# Testing yevm-wasm

## Options

| Method | Runtime | Requires browser | Best for |
|---|---|---|---|
| `wasm-pack test --node` | Node.js | No | Unit tests, CI |
| `wasm-pack test --headless --chrome` | Chrome (headless) | Yes (installed) | Browser API tests |
| `wasm-pack test --headless --firefox` | Firefox (headless) | Yes (installed) | Cross-browser checks |
| Vitest + `@vitest/browser` | Browser / Node | Optional | JS-side integration tests |

### When to use what

**`wasm-pack test --node`** — default choice for pure logic: serialization, encoding, EVM execution. Fast, no browser install, runs in CI without extra setup. Limitation: no browser globals (`window`, `crypto.subtle`, etc.).

**`wasm-pack test --headless --chrome/firefox`** — when your code uses browser APIs or you need to verify behaviour in an actual JS engine. Slower, requires the browser binary, but tests run in Rust so they stay close to the implementation.

**Vitest + `@vitest/browser`** — when you're testing the JS/TS API surface: how consumers call your functions, JSON shapes, error messages, TypeScript types. Tests are written in JS so they catch integration issues that Rust tests miss (wrong field names, unexpected `undefined`, etc.). Requires a build step (`wasm-pack build`) before each test run.

Use **Node** unless you need browser-specific APIs (`window`, `fetch`, DOM, etc.).

---

## Setup

### 1. Add dev dependency

```toml
# Cargo.toml
[dev-dependencies]
wasm-bindgen-test = "0.3"
```

### 2. Write tests

```rust
// src/lib.rs (or src/tests.rs)
#[cfg(test)]
mod tests {
    use wasm_bindgen_test::*;
    use super::*;

    // For Node: no configure macro needed.
    // For browser: uncomment the line below.
    // wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    fn test_hello() {
        let result = hello("world".into());
        assert_eq!(result, "Hello, world");
    }

    #[wasm_bindgen_test]
    fn test_call_roundtrip() {
        use js_sys::JSON;
        let json = JSON::parse(r#"{"by":"0xaa","to":"0xbb","eth":"0x0","gas":21000,"data":"0x"}"#).unwrap();
        let result = call(json);
        assert!(result.is_ok());
    }
}
```

### 3. Run

```sh
# Node (no browser needed)
wasm-pack test --release --node yevm-wasm

# Headless Chrome
wasm-pack test --release --headless --chrome yevm-wasm

# Headless Firefox
wasm-pack test --release --headless --firefox yevm-wasm
```

Run from the workspace root or replace `yevm-wasm` with `.` if running from inside the crate directory.

---

## Vitest + @vitest/browser

Tests are written in JS/TS and import the built wasm package. Good fit if you already have a JS frontend or want to test the JS API surface directly.

### 1. Setup

```sh
npm init -y
npm install -D vitest @vitest/browser vite-plugin-wasm
```

```js
// vitest.config.js
import { defineConfig } from 'vitest/config'
import wasm from 'vite-plugin-wasm'

export default defineConfig({
  plugins: [wasm()],
  test: {
    browser: {
      enabled: true,
      name: 'chromium',
      provider: 'playwright',
    },
  },
})
```

### 2. Build the wasm package first

```sh
wasm-pack build --target web yevm-wasm
```

### 3. Write tests

```js
// tests/call.test.js
import init, { call } from '../yevm-wasm/pkg/yevm_wasm.js'
import { beforeAll, expect, test } from 'vitest'

beforeAll(() => init())

test('call roundtrip', () => {
  const result = call({ by: '0xaa', to: '0xbb', eth: '0x0', gas: 21000, data: '0x' })
  expect(result).toBeDefined()
})
```

### 4. Run

```sh
npx vitest run
```

---

## CI (GitHub Actions)

```yaml
- name: Install wasm-pack
  run: curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh

- name: Test wasm (node)
  run: wasm-pack test --release --node yevm-wasm
```

No browser install needed for the Node path.
