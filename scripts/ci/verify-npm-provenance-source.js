#!/usr/bin/env node
'use strict'

const fs = require('fs')

const SLSA_PROVENANCE_V1 = 'https://slsa.dev/provenance/v1'

function verifyNpmProvenanceSource(document, { commitSha, refName, repository }) {
  if (!/^[0-9a-f]{40}$/u.test(commitSha)) throw new Error('Expected a full lowercase Git commit SHA')
  if (typeof refName !== 'string' || refName.length === 0) throw new Error('Expected a tag ref name')
  if (typeof repository !== 'string' || !repository.startsWith('https://github.com/')) {
    throw new Error('Expected a GitHub HTTPS repository URL')
  }

  const attestation = document?.attestations?.find(item => item?.predicateType === SLSA_PROVENANCE_V1)
  const encodedPayload = attestation?.bundle?.dsseEnvelope?.payload
  if (typeof encodedPayload !== 'string' || encodedPayload.length === 0) {
    throw new Error('npm provenance bundle is missing its DSSE payload')
  }

  const statement = JSON.parse(Buffer.from(encodedPayload, 'base64').toString('utf8'))
  const workflow = statement?.predicate?.buildDefinition?.externalParameters?.workflow
  const resolvedDependencies = statement?.predicate?.buildDefinition?.resolvedDependencies
  const expectedRef = `refs/tags/${refName}`
  const expectedUri = `git+${repository}@${expectedRef}`

  if (statement?.predicateType !== SLSA_PROVENANCE_V1) {
    throw new Error('npm provenance statement is not SLSA provenance v1')
  }
  if (workflow?.repository !== repository || workflow?.ref !== expectedRef) {
    throw new Error('npm provenance workflow repository/ref does not match this release tag')
  }
  if (
    !Array.isArray(resolvedDependencies) ||
    !resolvedDependencies.some(
      dependency => dependency?.uri === expectedUri && dependency?.digest?.gitCommit === commitSha
    )
  ) {
    throw new Error('npm provenance source commit does not match this release tag commit')
  }
}

if (require.main === module) {
  const [documentPath, commitSha, refName, repository] = process.argv.slice(2)
  if ([documentPath, commitSha, refName, repository].some(value => value === undefined)) {
    throw new Error(
      'Usage: verify-npm-provenance-source.js <attestations.json> <commit-sha> <tag-name> <repository-url>'
    )
  }
  verifyNpmProvenanceSource(JSON.parse(fs.readFileSync(documentPath, 'utf8')), {
    commitSha,
    refName,
    repository
  })
}

module.exports = Object.freeze({ SLSA_PROVENANCE_V1, verifyNpmProvenanceSource })
