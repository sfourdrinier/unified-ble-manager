const fs = require('fs')
const path = require('path')
const ts = require('typescript')

const root = path.resolve(__dirname, '..')

describe('TypeScript smell guards', () => {
  test('host identity hashing has no inferred any array or non-null assertions', () => {
    const filename = path.join(root, 'src/public/host-identity.ts')
    const sourceText = fs.readFileSync(filename, 'utf8')
    const source = ts.createSourceFile(filename, sourceText, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS)
    const program = ts.createProgram([filename], { strict: true, noUncheckedIndexedAccess: true, target: ts.ScriptTarget.ESNext })
    const checker = program.getTypeChecker()
    let workArrayType = null
    let nonNullAssertions = 0

    function visit(node) {
      if (ts.isVariableDeclaration(node) && node.name.getText(source) === 'W') {
        workArrayType = checker.typeToString(checker.getTypeAtLocation(node.name))
      }
      if (ts.isNonNullExpression(node)) nonNullAssertions += 1
      ts.forEachChild(node, visit)
    }
    visit(source)

    expect(workArrayType).not.toBe('any[]')
    expect(nonNullAssertions).toBe(0)
  })
})
