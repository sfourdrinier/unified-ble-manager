const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '..')

function collectSourceFiles(directory, files = []) {
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    const full = path.join(directory, entry.name)
    if (entry.isDirectory()) collectSourceFiles(full, files)
    else if (/\.(ts|tsx|js)$/.test(entry.name)) files.push(full)
  }
  return files
}

describe('plugin runtime boundary', () => {
  test('plugin sources import Node builtins with the node: prefix', () => {
    const production = collectSourceFiles(path.join(root, 'plugin/src'))
    expect(production.length).toBeGreaterThan(0)
    for (const file of production) {
      const source = fs.readFileSync(file, 'utf8')
      expect(source).not.toMatch(/from ['"]crypto['"]/)
      expect(source).not.toMatch(/from ['"]fs['"]/)
      expect(source).not.toMatch(/from ['"]path['"]/)
      expect(source).not.toMatch(/from ['"]os['"]/)
      expect(source).not.toMatch(/require\(['"]crypto['"]\)/)
    }
  })

  test('runtime sources never import the Expo config plugin tree', () => {
    const runtimeFiles = collectSourceFiles(path.join(root, 'src'))
    expect(runtimeFiles.length).toBeGreaterThan(0)
    for (const file of runtimeFiles) {
      const source = fs.readFileSync(file, 'utf8')
      expect(source).not.toMatch(/from ['"][^'"]*plugin\//)
      expect(source).not.toMatch(/require\(['"][^'"]*plugin\//)
    }
  })

  test('package runtime entrypoints do not resolve into plugin/build', () => {
    const entrypoints = [
      'src/index.ts',
      'src/react.ts',
      'src/react-native.ts',
      'src/web.ts',
      'src/expo.ts',
      'src/electron-main.ts',
      'src/electron-renderer.ts'
    ]
    for (const relative of entrypoints) {
      const source = fs.readFileSync(path.join(root, relative), 'utf8')
      expect(source).not.toMatch(/plugin\/build/)
      expect(source).not.toMatch(/app\.plugin/)
    }
  })
})
