export type ExportFormat = 'json' | 'csv'

export function metadataExportFilename(source: string, sessionId: string, format: ExportFormat) {
  return `brick-metadata-${source}-${sessionId}.${format}`
}
