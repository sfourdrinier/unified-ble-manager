'use strict'

const { verifyNpmProvenanceSource } = require('../scripts/ci/verify-npm-provenance-source')

const commitSha = 'a'.repeat(40)
const refName = 'v4.0.0'
const repository = 'https://github.com/sfourdrinier/unified-ble-manager'

function attestation() {
  const statement = {
    predicateType: 'https://slsa.dev/provenance/v1',
    predicate: {
      buildDefinition: {
        externalParameters: { workflow: { repository, ref: `refs/tags/${refName}` } },
        resolvedDependencies: [
          {
            uri: `git+${repository}@refs/tags/${refName}`,
            digest: { gitCommit: commitSha }
          }
        ]
      }
    }
  }
  return {
    attestations: [
      {
        predicateType: 'https://slsa.dev/provenance/v1',
        bundle: { dsseEnvelope: { payload: Buffer.from(JSON.stringify(statement)).toString('base64') } }
      }
    ]
  }
}

describe('npm provenance source binding', () => {
  test('accepts only the exact repository, tag ref, and source commit', () => {
    expect(() => verifyNpmProvenanceSource(attestation(), { commitSha, refName, repository })).not.toThrow()

    const wrongCommit = attestation()
    const payload = JSON.parse(
      Buffer.from(wrongCommit.attestations[0].bundle.dsseEnvelope.payload, 'base64').toString('utf8')
    )
    payload.predicate.buildDefinition.resolvedDependencies[0].digest.gitCommit = 'b'.repeat(40)
    wrongCommit.attestations[0].bundle.dsseEnvelope.payload = Buffer.from(JSON.stringify(payload)).toString('base64')
    expect(() => verifyNpmProvenanceSource(wrongCommit, { commitSha, refName, repository })).toThrow('source commit')
  })
})
