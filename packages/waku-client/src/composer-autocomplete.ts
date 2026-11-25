import type {
  FileEntry,
  ProviderKind,
  ProviderModel,
  ProviderModelOption,
  ReportedCommand,
  SlashCommand,
} from './generated'
import { fuzzyScore } from './fuzzy-search'

export const COMPOSER_AUTOCOMPLETE_CAP = 64

export type ComposerTriggerKind = 'command' | 'file'

export interface ComposerTrigger {
  kind: ComposerTriggerKind
  query: string
  start: number
  end: number
}

export type ComposerAutocompleteRow =
  | { kind: 'command'; command: SlashCommand }
  | { kind: 'file'; file: FileEntry }

export function detectComposerTrigger(text: string, cursor: number): ComposerTrigger | null {
  const end = Math.max(0, Math.min(cursor, text.length))
  const lineStart = text.lastIndexOf('\n', end - 1) + 1
  const linePrefix = text.slice(lineStart, end)
  if (linePrefix.startsWith('/')) {
    const query = linePrefix.slice(1)
    if (/\s/u.test(query)) return null
    return { kind: 'command', query, start: lineStart, end }
  }

  let tokenStart = end
  while (tokenStart > 0 && !/\s/u.test(text[tokenStart - 1]!)) tokenStart -= 1
  const token = text.slice(tokenStart, end)
  if (!token.startsWith('@')) return null
  return { kind: 'file', query: token.slice(1), start: tokenStart, end }
}

export function mergeComposerCommands(
  discovered: SlashCommand[],
  reported: ReportedCommand[],
): SlashCommand[] {
  const merged = discovered.map((command) => ({ ...command }))
  for (const report of reported) {
    const known = merged.find((command) => command.name === report.name)
    if (known) {
      if (known.scope === 'Waku') continue
      if (!known.description) known.description = report.description ?? ''
      if (known.argument_hint == null) known.argument_hint = report.argument_hint ?? null
      continue
    }
    merged.push({
      name: report.name,
      description: report.description ?? '',
      scope: 'Builtin',
      argument_hint: report.argument_hint ?? null,
      template: null,
    })
  }
  return merged.sort((left, right) => {
    const byScope = commandScopeRank(left.scope) - commandScopeRank(right.scope)
    return byScope || left.name.localeCompare(right.name)
  })
}

export function isFastModeToggleSubmission(
  provider: ProviderKind,
  prompt: string,
  commands: SlashCommand[],
): boolean {
  return provider === 'codex'
    && prompt.trim() === '/fast'
    && commands.some((command) => command.name === 'fast'
      && command.scope === 'Builtin'
      && command.template === null)
}

/** Waku's provider-neutral terminal-session picker. */
export function isResumeSubmission(prompt: string): boolean {
  return prompt.trim() === '/resume'
}

export type GoalCommand =
  | { kind: 'show' }
  | { kind: 'edit' }
  | { kind: 'pause' }
  | { kind: 'resume' }
  | { kind: 'clear' }
  | { kind: 'set'; objective: string }

/**
 * Parse the submitted text as Codex's native `/goal` command, which Waku
 * bridges to `thread/goal/*`. `null` when it is not one — wrong provider,
 * other text, or a project/user command that deliberately owns `/goal`
 * (resolution precedence stands).
 */
export function parseGoalSubmission(
  provider: ProviderKind,
  prompt: string,
  commands: SlashCommand[],
): GoalCommand | null {
  if (provider !== 'codex') return null
  const invocation = prompt.trim()
  if (!invocation.startsWith('/')) return null
  const body = invocation.slice(1)
  const split = body.match(/^(\S+)(?:\s+([\s\S]*))?$/u)
  if (!split || split[1] !== 'goal') return null
  const goalIsCodexBuiltin = commands.some((command) => command.name === 'goal'
    && command.scope === 'Builtin'
    && command.template === null)
  if (!goalIsCodexBuiltin) return null
  const argument = (split[2] ?? '').trim()
  switch (argument) {
    case '': return { kind: 'show' }
    case 'edit': return { kind: 'edit' }
    case 'pause': return { kind: 'pause' }
    case 'resume': return { kind: 'resume' }
    case 'clear': return { kind: 'clear' }
    default: return { kind: 'set', objective: argument }
  }
}

export function isCodexUsageSubmission(
  provider: ProviderKind,
  prompt: string,
  commands: SlashCommand[],
): boolean {
  return provider === 'codex'
    && prompt.trim() === '/usage'
    && !commandIsOverridden('usage', '', commands)
}

export type AntigravityLocalCommand =
  | { kind: 'usage' }
  | { kind: 'modelPicker' }
  | { kind: 'setModel'; query: string }
  | { kind: 'effortShow' }
  | { kind: 'setEffort'; query: string }
  | { kind: 'swarmBoard' }

export function antigravityLocalCommandExecutesOnAccept(
  command: AntigravityLocalCommand,
): boolean {
  return command.kind !== 'effortShow'
}

/**
 * Parse the submitted text as an Antigravity CLI-local command bridged by Waku
 * without starting an ACP turn. Returns `null` for other providers, agent-side
 * commands (`/boost`, `/swarm <task>`, `/plan <task>`, etc.), or when a
 * project/user/skill/template command overrides the name.
 */
export function parseAntigravityLocalCommand(
  provider: ProviderKind,
  prompt: string,
  commands: SlashCommand[],
): AntigravityLocalCommand | null {
  if (provider !== 'antigravity') return null
  const invocation = prompt.trim()
  if (!invocation.startsWith('/')) return null
  const body = invocation.slice(1)
  const split = body.match(/^(\S+)(?:\s+([\s\S]*))?$/u)
  if (!split) return null
  const name = split[1]!
  const argument = (split[2] ?? '').trim()
  if (commandIsOverridden(name, argument, commands)) return null
  switch (name) {
    case 'usage':
    case 'quota':
    case 'credits':
      return argument === '' ? { kind: 'usage' } : null
    case 'model':
      return argument === '' ? { kind: 'modelPicker' } : { kind: 'setModel', query: argument }
    case 'effort':
      return argument === '' ? { kind: 'effortShow' } : { kind: 'setEffort', query: argument }
    case 'swarm':
      return argument === 'board' ? { kind: 'swarmBoard' } : null
    default:
      return null
  }
}

export function resolveProviderModel(
  models: readonly ProviderModel[],
  query: string,
): ProviderModel | null {
  const trimmed = query.trim()
  if (!trimmed) return null
  const lower = trimmed.toLocaleLowerCase()
  const byId = models.find((model) => model.id.toLocaleLowerCase() === lower)
  if (byId) return byId
  const byName = models.find((model) => model.name.toLocaleLowerCase() === lower)
  if (byName) return byName
  const bySlug = models.find(
    (model) => model.name.toLocaleLowerCase().split(/\s+/u).filter(Boolean).join('-') === lower,
  )
  if (bySlug) return bySlug
  const matches = models.filter(
    (model) => model.id.toLocaleLowerCase().includes(lower)
      || model.name.toLocaleLowerCase().includes(lower),
  )
  return matches.length === 1 ? matches[0]! : null
}

export function resolveProviderEffort(
  options: readonly ProviderModelOption[],
  query: string,
): ProviderModelOption | null {
  const trimmed = query.trim()
  if (!trimmed) return null
  const lower = trimmed.toLocaleLowerCase()
  const slugLower = lower.split(/\s+/u).filter(Boolean).join('-')
  const byId = options.find((option) => option.id.toLocaleLowerCase() === lower)
  if (byId) return byId
  const byLabel = options.find((option) => option.label.toLocaleLowerCase() === lower)
  if (byLabel) return byLabel
  const bySlug = options.find(
    (option) => option.label.toLocaleLowerCase().split(/\s+/u).filter(Boolean).join('-') === slugLower,
  )
  if (bySlug) return bySlug
  const matches = options.filter(
    (option) => option.id.toLocaleLowerCase().includes(lower)
      || option.label.toLocaleLowerCase().includes(lower),
  )
  return matches.length === 1 ? matches[0]! : null
}

function commandIsOverridden(
  name: string,
  argument: string,
  commands: SlashCommand[],
): boolean {
  return commands.some((command) => {
    if (command.name !== name) return false
    if (command.template !== null && command.template !== undefined) return true
    if (name === 'swarm' && argument === 'board' && command.scope === 'Skill') return false
    return command.scope !== 'Builtin'
  })
}

export function toggledFastServiceTier(
  current: string | null | undefined,
  serviceTiers: ProviderModelOption[],
): string | null {
  const fast = serviceTiers.find((tier) => ['fast', 'priority'].includes(tier.id)
    || tier.label.toLocaleLowerCase() === 'fast')
  if (!fast) return null
  return current === fast.id ? 'default' : fast.id
}

export function composerAutocompleteRows(
  trigger: ComposerTrigger,
  commands: SlashCommand[],
  files: FileEntry[],
  cap = COMPOSER_AUTOCOMPLETE_CAP,
): ComposerAutocompleteRow[] {
  if (!trigger.query.trim()) {
    return trigger.kind === 'command'
      ? commands.slice(0, cap).map((command) => ({ kind: 'command', command }))
      : files.slice(0, cap).map((file) => ({ kind: 'file', file }))
  }
  const source = trigger.kind === 'command'
    ? commands.map((command) => ({
        row: { kind: 'command' as const, command },
        candidate: command.name,
      }))
    : files.map((file) => ({
        row: { kind: 'file' as const, file },
        candidate: file.path,
      }))
  return source
    .map(({ row, candidate }, index) => ({
      row,
      index,
      score: fuzzyScore(trigger.query, candidate),
    }))
    .filter((item): item is typeof item & { score: number } => item.score !== null)
    .sort((left, right) => right.score - left.score || left.index - right.index)
    .slice(0, cap)
    .map(({ row }) => row)
}

export function replaceComposerTrigger(
  text: string,
  trigger: ComposerTrigger,
  row: ComposerAutocompleteRow,
): { text: string; cursor: number } {
  const insert = row.kind === 'command'
    ? `/${row.command.name} `
    : `@${row.file.path} `
  const next = `${text.slice(0, trigger.start)}${insert}${text.slice(trigger.end)}`
  return { text: next, cursor: trigger.start + insert.length }
}

export function expandCommandTemplate(template: string, args: string): string {
  const positional = args.split(/\s+/u).filter(Boolean)
  let expanded = ''
  let consumedArgs = false
  let rest = template
  while (true) {
    const index = rest.indexOf('$')
    if (index < 0) break
    expanded += rest.slice(0, index)
    const after = rest.slice(index + 1)
    if (after.startsWith('ARGUMENTS')) {
      expanded += args
      consumedArgs = true
      rest = after.slice('ARGUMENTS'.length)
    } else if (after.startsWith('@')) {
      expanded += args
      consumedArgs = true
      rest = after.slice(1)
    } else if (/^[1-9]/u.test(after)) {
      expanded += positional[Number(after[0]) - 1] ?? ''
      consumedArgs = true
      rest = after.slice(1)
    } else {
      expanded += '$'
      rest = after
    }
  }
  expanded += rest
  if (!consumedArgs && args) expanded += `\n\n${args}`
  return expanded
}

export function expandedComposerSubmission(
  provider: ProviderKind,
  prompt: string,
  commands: SlashCommand[],
): string | null {
  if (!prompt.startsWith('/')) return null
  const invocation = prompt.slice(1)
  const whitespace = invocation.search(/\s/u)
  const name = whitespace < 0 ? invocation : invocation.slice(0, whitespace)
  const args = whitespace < 0 ? '' : invocation.slice(whitespace).trim()
  const skill = commands.find((item) => item.name === name && item.scope === 'Skill')
  if (skill) {
    if (provider === 'codex' || provider === 'fx') return `$${invocation}`
    if (provider === 'pi' || provider === 'ohMyPi') return `/skill:${invocation}`
  }
  const command = commands.find((item) => item.name === name && item.template !== null)
  return command?.template === null || command?.template === undefined
    ? null
    : expandCommandTemplate(command.template, args)
}

function commandScopeRank(scope: SlashCommand['scope']): number {
  switch (scope) {
    case 'Builtin':
    case 'Waku':
      return 0
    case 'Project':
      return 1
    case 'User':
      return 2
    case 'Skill':
      return 3
  }
}
