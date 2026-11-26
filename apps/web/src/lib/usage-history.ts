import type { ProviderDay, UsageHistory } from '@waku/client'

export const EMPTY_PROVIDER_DAY: ProviderDay = { costUsd: 0, totalTokens: 0 }

/** Older daemons and React Query caches contain only Claude and Codex. */
export function normalizeUsageHistory(history: UsageHistory): UsageHistory {
  const normalize = <T extends { byProvider: ProviderDay[] }>(row: T): T => ({
    ...row,
    byProvider: [0, 1, 2].map((index) => row.byProvider[index] ?? { ...EMPTY_PROVIDER_DAY }),
  })
  return {
    ...history,
    daily: history.daily.map(normalize),
    months: history.months.map(normalize),
    projects: history.projects.map(normalize),
  }
}
