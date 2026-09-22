// example-tauri/vite.config.mts
//
// Webview frontend for `cargo run` (debug builds load build.devUrl). Run from
// the repository root: `pnpm prepack && pnpm exec vite --config example-tauri/vite.config.mts`.
//
// example-tauri has its own package.json, so a bare `unified-ble-manager`
// import from here would look for an install that this checkout example does
// not need. `checkoutPackage` resolves every such import (from src/ and from
// the shared driver alike) as the repository's own package, the same single
// instance, exactly as example-web resolves it.

import { fileURLToPath } from 'node:url'
import { defineConfig, type Plugin } from 'vite'

const exampleRoot = fileURLToPath(new URL('.', import.meta.url))
const repositoryRoot = fileURLToPath(new URL('..', import.meta.url))
const insideRootPackage = fileURLToPath(new URL('../examples-shared/driver/index.ts', import.meta.url))
const PACKAGE = /^unified-ble-manager(\/|$)/

function checkoutPackage(): Plugin {
  return {
    name: 'ubm-checkout-package',
    enforce: 'pre',
    async resolveId(source, importer) {
      if (!PACKAGE.test(source) || importer === insideRootPackage) return null
      const resolved = await this.resolve(source, insideRootPackage, { skipSelf: true })
      if (resolved === null) throw new Error(`${source} did not resolve from the checkout root; run pnpm prepack`)
      return resolved
    }
  }
}

export default defineConfig({
  root: exampleRoot,
  plugins: [checkoutPackage()],
  clearScreen: false,
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2022',
    rollupOptions: {
      input: { main: fileURLToPath(new URL('index.html', import.meta.url)), driver: fileURLToPath(new URL('driver.html', import.meta.url)) }
    }
  },
  server: {
    host: '127.0.0.1',
    port: 1420,
    strictPort: true,
    fs: { allow: [repositoryRoot] },
    watch: { ignored: ['**/src-tauri/**'] }
  }
})
