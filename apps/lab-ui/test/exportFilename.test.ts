/// <reference types="node" />

import assert from 'node:assert/strict'
import test from 'node:test'
import { metadataExportFilename } from '../src/exportFilename.ts'

test('metadataExportFilename uses brick-metadata prefix', () => {
  assert.equal(metadataExportFilename('codex', 'session-123', 'json'), 'brick-metadata-codex-session-123.json')
  assert.equal(metadataExportFilename('cursor', 'session-456', 'csv'), 'brick-metadata-cursor-session-456.csv')
})
