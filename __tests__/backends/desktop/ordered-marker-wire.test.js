// __tests__/backends/desktop/ordered-marker-wire.test.js
const { decodeOrderedMarkerWriteStatus } = require('../../../src/core/ordered-marker-wire')
test('initiating remains distinct and wide marker ordinal remains exact', () => {
  const receipt = {state:'admitted',ordinal:'18446744073709551615',write:{state:'initiating'}}
  expect(decodeOrderedMarkerWriteStatus(receipt)).toEqual(receipt)
})
test.each([
  {state:'admitted',ordinal:'01',write:{state:'initiating'}},
  {state:'admitted',ordinal:'0',write:{state:'initiating',error:null}},
  {state:'rejected',error:null},
  {state:'pending',extra:true}
])('rejects malformed marker receipt %#', value => {
  expect(() => decodeOrderedMarkerWriteStatus(value)).toThrow('protocol.malformed')
})
