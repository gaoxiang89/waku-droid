import type {
  AgentSession,
  DaemonSettings,
  ProviderAccount,
  ProviderKind,
} from './generated'

export const MULTI_ACCOUNT_PROVIDERS: readonly ProviderKind[] = [
  'antigravity',
  'claude',
  'codex',
]

export function supportsMultipleAccounts(provider: ProviderKind): boolean {
  return MULTI_ACCOUNT_PROVIDERS.includes(provider)
}

export function resolveProviderAccount(
  settings: DaemonSettings | null | undefined,
  provider: ProviderKind,
  accountId: string | null | undefined,
): ProviderAccount | null {
  if (!settings || !accountId || !supportsMultipleAccounts(provider)) return null
  return settings.provider_accounts?.find(
    (account) => account.provider === provider && account.id === accountId,
  ) ?? null
}

export function validatedAccountId(
  settings: DaemonSettings | null | undefined,
  provider: ProviderKind,
  accountId: string | null | undefined,
): string | null {
  return resolveProviderAccount(settings, provider, accountId)?.id ?? null
}

export interface ProviderExecutionConfig {
  account: ProviderAccount | null
  accountId: string | null
  binaryOverride: string | null
  configDir: string | null
}

export function resolveProviderExecutionConfig(
  settings: DaemonSettings | null | undefined,
  provider: ProviderKind,
  accountId: string | null | undefined,
): ProviderExecutionConfig {
  const account = resolveProviderAccount(settings, provider, accountId)
  const binaryOverride = account?.binaryOverride?.trim()
    || settings?.provider_binary_overrides?.[provider]
    || null
  return {
    account,
    accountId: account?.id ?? null,
    binaryOverride,
    configDir: account?.configDir ?? null,
  }
}

export function providerProbeConfigKey(
  binaryOverride: string | null | undefined,
  configDir: string | null | undefined,
): string | null {
  const binary = binaryOverride ?? null
  return configDir ? `${binary ?? ''}|${configDir}` : binary
}

export function isWindowsAbsolutePath(value: string): boolean {
  return /^[a-z]:[\\/]/iu.test(value.trim())
}

export function normalizeAccountConfigDirKey(
  rawPath: string,
  caseInsensitive = isWindowsAbsolutePath(rawPath),
): string {
  let normalized = rawPath.trim().replaceAll('\\', '/')
  while (normalized.length > 1 && normalized.endsWith('/')) {
    normalized = normalized.slice(0, -1)
  }
  return caseInsensitive ? normalized.toLocaleLowerCase() : normalized
}

export function defaultAccountExampleDir(provider: ProviderKind): string {
  switch (provider) {
    case 'antigravity':
      return '~/.gemini/antigravity-work'
    case 'claude':
      return '~/.claude-work'
    case 'codex':
      return '~/.codex-work'
    default:
      return '~/.config-work'
  }
}

export function defaultAccountTildeDir(provider: ProviderKind): string {
  switch (provider) {
    case 'antigravity':
      return '~/.gemini/antigravity-cli'
    case 'claude':
      return '~/.claude'
    case 'codex':
      return '~/.codex'
    default:
      return '~'
  }
}

export function abbreviateHomePath(path: string): string {
  return path
    .replace(/^\/Users\/[^/]+(?=\/|$)/, '~')
    .replace(/^\/home\/[^/]+(?=\/|$)/, '~')
    .replace(/^\/root(?=\/|$)/, '~')
    .replace(/^[A-Za-z]:[\\/]Users[\\/][^\\/]+(?=[\\/]|$)/, '~')
}

function quotePosixShellArg(value: string): string {
  if (value === '~') return '~'
  if (value.startsWith('~/')) {
    const rest = value.slice(2)
    return rest ? `~/'${rest.replaceAll("'", "'\\''")}'` : '~/'
  }
  return `'${value.replaceAll("'", "'\\''")}'`
}

function quotePowershellArg(value: string): string {
  if (value === '~') return '$HOME'
  if (/^~[\\/]/u.test(value)) {
    return `(Join-Path $HOME '${value.slice(2).replaceAll("'", "''")}')`
  }
  return `'${value.replaceAll("'", "''")}'`
}

export function formatAccountLoginCommand(
  provider: ProviderKind,
  configDir: string,
  windowsShell = isWindowsAbsolutePath(configDir),
): string {
  const trimmed = configDir.trim()
  const quoted = windowsShell ? quotePowershellArg(trimmed) : quotePosixShellArg(trimmed)
  switch (provider) {
    case 'antigravity':
      return `agy acp --gemini-dir ${quoted} --app-data-dir . --login`
    case 'claude':
      return windowsShell
        ? `$env:CLAUDE_CONFIG_DIR=${quoted}; claude auth login`
        : `CLAUDE_CONFIG_DIR=${quoted} claude auth login`
    case 'codex':
      return windowsShell
        ? `$env:CODEX_HOME=${quoted}; codex login`
        : `CODEX_HOME=${quoted} codex login`
    default:
      return ''
  }
}

/**
 * Parks the active agent's resume cursor and adopts any parked cursor for the
 * incoming `(provider, accountId)`, mirroring Rust's `AgentSession::switch_agent`.
 */
export function switchSessionAgent(
  session: AgentSession,
  provider: ProviderKind,
  accountId: string | null | undefined,
  updatedAt = Math.floor(Date.now() / 1_000),
): AgentSession {
  const trimmedAccount = accountId?.trim() || null
  const nextAccountId = trimmedAccount && supportsMultipleAccounts(provider) ? trimmedAccount : null
  const outgoingProvider = session.provider
  const outgoingAccount = session.account_id ?? null
  const agentChanged = outgoingProvider !== provider || outgoingAccount !== nextAccountId

  const turns = agentChanged
    ? session.turns.map((turn) => (turn.provider == null
      ? { ...turn, provider: outgoingProvider, account_id: outgoingAccount }
      : turn))
    : session.turns

  let providerCursors = [...(session.provider_cursors ?? [])]
  let accountCursors = [...(session.account_cursors ?? [])]

  if (session.provider_cursor) {
    const cursor = session.provider_cursor
    const cursorProvider = cursor.provider
    if (outgoingAccount) {
      accountCursors = [
        ...accountCursors.filter(
          (parked) => !(parked.cursor.provider === cursorProvider && parked.accountId === outgoingAccount),
        ),
        { accountId: outgoingAccount, cursor },
      ]
    } else {
      providerCursors = [
        ...providerCursors.filter((parked) => parked.provider !== cursorProvider),
        cursor,
      ]
    }
  }

  let nextCursor = null
  if (nextAccountId) {
    const index = accountCursors.findIndex(
      (entry) => entry.cursor.provider === provider && entry.accountId === nextAccountId,
    )
    if (index >= 0) {
      nextCursor = accountCursors[index]!.cursor
      accountCursors = [...accountCursors.slice(0, index), ...accountCursors.slice(index + 1)]
    }
  } else {
    const index = providerCursors.findIndex((cursor) => cursor.provider === provider)
    if (index >= 0) {
      nextCursor = providerCursors[index]!
      providerCursors = [...providerCursors.slice(0, index), ...providerCursors.slice(index + 1)]
    }
  }

  return {
    ...session,
    provider,
    account_id: nextAccountId,
    provider_cursor: nextCursor,
    provider_cursors: providerCursors,
    account_cursors: accountCursors,
    turns,
    ...(agentChanged
      ? {
          thread_goal: null,
          context_usage: null,
          available_commands: [],
          provider_session_id: null,
          updated_at: updatedAt,
        }
      : {}),
  }
}
