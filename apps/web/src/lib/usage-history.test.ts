import { describe, expect, test } from 'bun:test'
import type { UsageHistory } from '@waku/client'
import { normalizeUsageHistory } from './usage-history'

function history(byProvider: Array<{ costUsd: number, totalTokens: number }>): UsageHistory {
  const row = {
    day: '2026-01-02',
    costUsd: 3,
    totalTokens: 30,
    byProvider,
  }
  return {
    window: { trailingDays: 7 },
    sinceDay: '2026-01-01',
    untilDay: '2026-01-02',
    totals: { uncachedInput: 0, cachedInput: 0, cacheCreation: 0, output: 0, reasoning: 0 },
    totalTokens: 30,
    costUsd: 3,
    records: 1,
    sessions: 1,
    providers: [],
    models: [],
    daily: [row as unknown as UsageHistory['daily'][number]],
    months: [{
      firstDay: '2026-01-01',
      costUsd: 3,
      totalTokens: 30,
      byProvider,
      sessions: 1,
      activeDays: 1,
      topModels: [],
    } as unknown as UsageHistory['months'][number]],
    projects: [{
      path: '/tmp/p',
      costUsd: 3,
      totalTokens: 30,
      byProvider,
      sessions: 1,
      costShare: 1,
      lastDay: '2026-01-02',
      topModels: [],
    } as unknown as UsageHistory['projects'][number]],
    quality: { providerReportedShare: 0, modelPricedShare: 0, unpricedShare: 0, cacheSavingsUsd: 0 },
    pricing: 'fresh',
    scannedFiles: 1,
    skippedFiles: 0,
    errors: [],
    scanDuration: { secs: 0, nanos: 0 },
  }
}

describe('normalizeUsageHistory', () => {
  test('old two-provider history does not throw when the chart reads the third provider', () => {
    const normalized = normalizeUsageHistory(history([
      { costUsd: 1, totalTokens: 10 },
      { costUsd: 2, totalTokens: 20 },
    ]))
    for (const row of [normalized.daily[0]!, normalized.months[0]!, normalized.projects[0]!]) {
      expect(row.byProvider).toHaveLength(3)
      expect(row.byProvider[2]).toEqual({ costUsd: 0, totalTokens: 0 })
      expect(row.byProvider[2]!.costUsd).toBe(0)
    }
    expect(normalized.daily[0]!.byProvider[0]!.totalTokens).toBe(10)
    expect(normalized.daily[0]!.byProvider[1]!.totalTokens).toBe(20)
  })

  test('a payload that already has the third provider is kept', () => {
    const normalized = normalizeUsageHistory(history([
      { costUsd: 1, totalTokens: 10 },
      { costUsd: 2, totalTokens: 20 },
      { costUsd: 3, totalTokens: 30 },
    ]))
    expect(normalized.daily[0]!.byProvider[2]!.totalTokens).toBe(30)
  })

  test('a payload missing every provider still decodes as zeros', () => {
    const normalized = normalizeUsageHistory(history([]))
    expect(normalized.projects[0]!.byProvider.map((day) => day.totalTokens)).toEqual([0, 0, 0])
  })
})
