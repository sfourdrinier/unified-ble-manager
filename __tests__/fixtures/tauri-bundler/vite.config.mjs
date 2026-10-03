import { createLogger, defineConfig } from 'vite'

const logger = createLogger()
logger.warn = message => {
  throw new Error(`Packed Tauri bundle warning: ${message}`)
}
logger.warnOnce = logger.warn

export default defineConfig({ customLogger: logger })
