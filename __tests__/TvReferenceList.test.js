const React = require('react')
const fs = require('node:fs')
const path = require('node:path')
const ts = require('typescript')
const { StyleSheet } = require('react-native')
const native = { FlatList: 'FlatList', Platform: { isTV: false }, View: 'View' }
const { FlatList, Platform, View } = native
let mockHeight = null
const mockSetHeight = jest.fn()
// Execute the real TSX with deterministic React hooks/native primitives. The
// app has an independent node_modules tree; it must not leak a second React
// instance into a root-package unit runner.
const source = fs.readFileSync(
  path.join(__dirname, '../example-expo/src/components/atoms/ReferenceFlatList/ReferenceFlatList.tsx'),
  'utf8'
)
const compiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.React, esModuleInterop: true }
}).outputText
const moduleUnderTest = { exports: {} }
const dependencies = name => {
  if (name === 'react') return { ...React, useState: () => [mockHeight, mockSetHeight] }
  if (name === 'react-native') return native
  throw new Error(`Unexpected reference-list dependency ${name}`)
}
new Function('require', 'module', 'exports', compiled)(dependencies, moduleUnderTest, moduleUnderTest.exports)
const { ReferenceFlatList } = moduleUnderTest.exports

afterEach(() => {
  mockHeight = null
  mockSetHeight.mockClear()
})

test('phone rendering preserves the standard virtualized list and its supplied props', () => {
  Platform.isTV = false
  const header = React.createElement(View)
  const props = { data: [], renderItem: () => null, ListHeaderComponent: header, style: { flex: 1 } }
  const result = ReferenceFlatList(props)
  expect(result.type).toBe(FlatList)
  expect(result.props.ListHeaderComponent).toBe(header)
  expect(result.props.style).toEqual({ flex: 1 })
})

test('TV list receives the measured viewport and resizes without forking content', () => {
  Platform.isTV = true
  const header = React.createElement(View)
  const props = { data: [], renderItem: () => null, ListHeaderComponent: header, style: { flex: 1 } }
  const first = ReferenceFlatList(props)
  expect(first.type).toBe(View)
  first.props.onLayout({ nativeEvent: { layout: { height: 935 } } })
  expect(mockSetHeight).toHaveBeenCalledWith(935)
  mockHeight = 935
  const result = ReferenceFlatList(props)
  expect(result.props.children.type).toBe(FlatList)
  expect(result.props.children.props.ListHeaderComponent).toBe(header)
  expect(StyleSheet.flatten(result.props.children.props.style)).toEqual({ flex: 0, height: 935 })
  result.props.onLayout({ nativeEvent: { layout: { height: 800 } } })
  expect(mockSetHeight).toHaveBeenLastCalledWith(800)
})
