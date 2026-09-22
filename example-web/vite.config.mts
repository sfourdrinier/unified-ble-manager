import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vite'

const page = (name: string) => fileURLToPath(new URL(name, import.meta.url))

export default defineConfig({
  root: 'example-web',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2022',
    rollupOptions: {
      // index.html: the Web Bluetooth walkthrough; driver.html: the shared test-driver host.
      input: { main: page('index.html'), driver: page('driver.html') }
    }
  },
  server: {
    host: '127.0.0.1',
    port: 5173,
    strictPort: true
  }
})
