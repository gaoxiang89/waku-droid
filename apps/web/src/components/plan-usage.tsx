import type { AgentSession, PlanUsage } from '@waku/client'
import { WakuIcon } from '@/components/waku-icon'
import { cn } from '@/lib/utils'

export type PlanUsageTranslator = (key: string, params?: Record<string, string | number>) => string

export const PLAN_USAGE_PROVIDERS: AgentSession['provider'][] = [
  'claude',
  'codex',
  'openCode',
  'grok',
  'antigravity',
]

export function isManualOnlyPlanUsage(provider: AgentSession['provider']): boolean {
  return provider === 'antigravity'
}

export function PlanUsageLanes({
  plan,
  provider,
  accountLabel,
  locale,
  t,
  showHeader = true,
}: {
  plan: PlanUsage
  provider: AgentSession['provider']
  accountLabel?: string | null
  locale: string
  t: PlanUsageTranslator
  showHeader?: boolean
}) {
  const usageUrl = provider === 'claude'
    ? 'https://claude.ai/settings/usage'
    : provider === 'codex'
      ? 'https://chatgpt.com/codex/settings/usage'
      : null
  const baseHeader = plan.planLabel
    ? t('usage.plan_limits_named', { plan: plan.planLabel })
    : t('usage.plan_limits')
  const header = accountLabel ? `${baseHeader} · ${accountLabel}` : baseHeader
  return (
    <>
      {showHeader && (usageUrl ? (
        <a
          className="flex min-w-0 items-center gap-1 text-[11px] text-[var(--text-tertiary)] outline-none hover:text-[var(--text-secondary)] focus-visible:ring-1 focus-visible:ring-ring"
          href={usageUrl}
          rel="noreferrer"
          target="_blank"
        >
          <span className="min-w-0 flex-1 truncate">{header}</span>
          <WakuIcon className="size-2.5" name="arrowRight" />
        </a>
      ) : (
        <div className="truncate text-[11px] text-[var(--text-tertiary)]">{header}</div>
      ))}
      {plan.windows.map((window) => (
        <UsageLane
          key={`${window.label}-${window.resetsAt ?? 'none'}`}
          label={window.label}
          percent={window.percent}
          reset={window.resetsAt == null ? undefined : resetLabel(window.resetsAt, locale, t)}
          value={`${window.percent.toFixed(0)}%`}
        />
      ))}
    </>
  )
}

export function UsageLane({
  label,
  percent,
  reset,
  value,
}: {
  label: string
  percent: number
  reset?: string
  value: string
}) {
  return (
    <div className="space-y-[7px]">
      <div className="flex min-w-0 items-center gap-2">
        <span className="min-w-0 flex-1 truncate">{label}</span>
        {reset && <span className="shrink-0 text-[11px] text-[var(--text-tertiary)]">{reset}</span>}
        <span className="shrink-0 text-[11px] text-[var(--text-secondary)]">{value}</span>
      </div>
      <div className="h-[3px] overflow-hidden rounded-full bg-accent">
        <div
          className={cn(
            'h-full rounded-full bg-[var(--text-secondary)]',
            percent >= 80 && 'bg-[var(--warning)]',
            percent >= 95 && 'bg-destructive',
          )}
          style={{ width: `${Math.min(100, Math.max(0, percent))}%` }}
        />
      </div>
    </div>
  )
}

export function UsageSkeleton({ t }: { t: PlanUsageTranslator }) {
  return (
    <div aria-label={t('usage.plan_limits')} className="space-y-3" role="status">
      <div className="h-2.5 w-24 rounded bg-accent motion-safe:animate-pulse" />
      {[0, 1].map((row) => (
        <div className="space-y-2" key={row}>
          <div className="flex justify-between">
            <div className="h-2.5 w-20 rounded bg-accent motion-safe:animate-pulse" />
            <div className="h-2.5 w-10 rounded bg-accent motion-safe:animate-pulse" />
          </div>
          <div className="h-[3px] rounded-full bg-accent motion-safe:animate-pulse" />
        </div>
      ))}
    </div>
  )
}

export function resetLabel(resetsAt: number, locale: string, t: PlanUsageTranslator) {
  const seconds = resetsAt - Math.floor(Date.now() / 1_000)
  if (seconds <= 0) return t('usage.resets_soon')
  const minutes = Math.ceil(seconds / 60)
  if (minutes < 60) return t('usage.resets_in_minutes', { count: minutes })
  if (minutes < 24 * 60) {
    const hours = Math.floor(minutes / 60)
    const remainder = minutes % 60
    return remainder
      ? t('usage.resets_in_hours_minutes', { hours, minutes: remainder })
      : t('usage.resets_in_hours', { count: hours })
  }
  return t('usage.resets_date', {
    date: new Intl.DateTimeFormat(locale, {
      weekday: 'short',
      hour: 'numeric',
      minute: '2-digit',
    }).format(new Date(resetsAt * 1_000)),
  })
}
