// example-electron/driver/vite.config.mts — bundles the sandboxed renderer (loaded by main from dist/ over file://).
import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vite'

export default defineConfig({
  root: fileURLToPath(new URL('.', import.meta.url)),
  base: './',
  build: { outDir: 'dist', emptyOutDir: true, target: 'es2022' }
})
