import { describe, expect, test } from 'bun:test'
import type { AgentSession, DaemonSettings } from './generated'
import {
  abbreviateHomePath,
  formatAccountLoginCommand,
  normalizeAccountConfigDirKey,
  providerProbeConfigKey,
  resolveProviderExecutionConfig,
  supportsMultipleAccounts,
  switchSessionAgent,
} from './provider-accounts'
import { readProviderProbeCache, writeProviderProbeCache, type ProviderProbeResult } from './provider-probe-cache'

describe('provider accounts & path utilities', () => {
  test('identifies multi-account providers', () => {
    expect(supportsMultipleAccounts('antigravity')).toBe(true)
    expect(supportsMultipleAccounts('claude')).toBe(true)
    expect(supportsMultipleAccounts('codex')).toBe(true)
    expect(supportsMultipleAccounts('openCode')).toBe(false)
  })

  test('resolves account configDir and binaryOverride with provider fallback', () => {
    const settings = {
      disabled_providers: [],
      computer_use_enabled: false,
      computer_use_allowed_apps: [],
      provider_binary_overrides: { antigravity: '/usr/local/bin/agy' },
      provider_accounts: [
        {
          id: 'agy-work',
          provider: 'antigravity',
          label: 'Work',
          configDir: '~/.gemini/antigravity-work',
          binaryOverride: '/opt/agy-work',
        },
        {
          id: 'agy-personal',
          provider: 'antigravity',
          label: 'Personal',
          configDir: '~/.gemini/antigravity-personal',
        },
      ],
    } as DaemonSettings

    expect(resolveProviderExecutionConfig(settings, 'antigravity', 'agy-work')).toMatchObject({
      accountId: 'agy-work',
      binaryOverride: '/opt/agy-work',
      configDir: '~/.gemini/antigravity-work',
    })
    expect(resolveProviderExecutionConfig(settings, 'antigravity', 'agy-personal')).toMatchObject({
      accountId: 'agy-personal',
      binaryOverride: '/usr/local/bin/agy',
      configDir: '~/.gemini/antigravity-personal',
    })
    expect(resolveProviderExecutionConfig(settings, 'antigravity', 'deleted')).toMatchObject({
      accountId: null,
      binaryOverride: '/usr/local/bin/agy',
      configDir: null,
    })
    expect(providerProbeConfigKey('/usr/local/bin/agy', '~/.gemini/antigravity-work')).toBe(
      '/usr/local/bin/agy|~/.gemini/antigravity-work',
    )
  })

  test('abbreviates POSIX and Windows user home directories', () => {
    expect(abbreviateHomePath('/Users/alice/.gemini/antigravity-work')).toBe('~/.gemini/antigravity-work')
    expect(abbreviateHomePath('/home/alice/.claude')).toBe('~/.claude')
    expect(abbreviateHomePath('/root')).toBe('~')
    expect(abbreviateHomePath('C:\\Users\\alice\\.gemini\\antigravity-work')).toBe('~\\.gemini\\antigravity-work')
    expect(abbreviateHomePath('C:\\Users\\alice')).toBe('~')
    expect(abbreviateHomePath('D:/Users/bob/.agy')).toBe('~/.agy')
    expect(abbreviateHomePath('/opt/shared/agy')).toBe('/opt/shared/agy')
  })

  test('formats Antigravity sign-in commands for POSIX and Windows shells', () => {
    expect(formatAccountLoginCommand('antigravity', '~/.gemini/antigravity-work', false)).toBe(
      "agy acp --gemini-dir ~/'.gemini/antigravity-work' --app-data-dir . --login",
    )
    expect(formatAccountLoginCommand('antigravity', 'C:\\Users\\alice\\.gemini\\work', true)).toBe(
      "agy acp --gemini-dir 'C:\\Users\\alice\\.gemini\\work' --app-data-dir . --login",
    )
  })

  test('normalizes Windows and POSIX config directories for duplicate checks', () => {
    expect(normalizeAccountConfigDirKey('C:\\Users\\Alice\\.gemini\\work\\')).toBe('c:/users/alice/.gemini/work')
    expect(normalizeAccountConfigDirKey('/Users/Alice/.gemini/work/')).toBe('/Users/Alice/.gemini/work')
  })

  test('parks and restores per-account resume cursors across agent switches', () => {
    const baseSession = {
      id: 's1',
      title: 'Test',
      auto_title: null,
      pinned: false,
      archived: false,
      project_id: 'p1',
      workspace: { kind: 'local' },
      provider: 'antigravity',
      account_id: null,
      model: 'gemini-4-neon',
      runtime_mode: 'fullAccess',
      reasoning_effort: 'high',
      service_tier: null,
      context_window: null,
      agent_preset: null,
      status: 'idle',
      created_at: 1,
      updated_at: 1,
      last_reply_at: null,
      provider_cursor: { provider: 'antigravity', sessionId: 'default-agy' },
      provider_cursors: [],
      account_cursors: [],
      available_commands: [{ name: 'usage', description: '', argument_hint: null }],
      context_usage: { tokens: 100, window: 1000 },
      provider_session_id: 'default-agy',
      messages: [],
      transcript_blocks: [],
      turns: [
        {
          id: 't1',
          turn_count: 1,
          status: 'completed',
          provider_turn_started: true,
          started_at: 1,
          completed_at: 2,
          checkpoint: null,
        },
      ],
      queued_messages: [],
    } satisfies AgentSession

    const onWork = switchSessionAgent(baseSession, 'antigravity', 'agy-work', 10)
    expect(onWork.account_id).toBe('agy-work')
    expect(onWork.provider_cursor).toBeNull()
    expect(onWork.provider_cursors).toEqual([{ provider: 'antigravity', sessionId: 'default-agy' }])
    expect(onWork.turns[0]).toMatchObject({ provider: 'antigravity', account_id: null })
    expect(onWork.available_commands).toEqual([])
    expect(onWork.context_usage).toBeNull()

    const onWorkWithCursor = {
      ...onWork,
      provider_cursor: { provider: 'antigravity' as const, sessionId: 'work-agy' },
    }
    const backToDefault = switchSessionAgent(onWorkWithCursor, 'antigravity', null, 20)
    expect(backToDefault.account_id).toBeNull()
    expect(backToDefault.provider_cursor).toEqual({ provider: 'antigravity', sessionId: 'default-agy' })
    expect(backToDefault.account_cursors).toEqual([
      { accountId: 'agy-work', cursor: { provider: 'antigravity', sessionId: 'work-agy' } },
    ])
  })

  test('isolates provider probe cache per account key', () => {
    const map = new Map<string, string>()
    const storage = {
      getItem: (key: string) => map.get(key) ?? null,
      setItem: (key: string, value: string) => {
        map.set(key, value)
      },
    }
    const defaultProbe: ProviderProbeResult = {
      provider: 'antigravity',
      installed: true,
      path: '/bin/agy',
      models: [],
      agent_presets: [],
      version: '1.5.16',
    }
    const workProbe: ProviderProbeResult = {
      ...defaultProbe,
      path: '/opt/work/agy',
    }
    writeProviderProbeCache(storage, 'ws://host', 'antigravity', null, defaultProbe, 1000, null)
    writeProviderProbeCache(storage, 'ws://host', 'antigravity', '/opt/work/agy|~/.agy-work', workProbe, 1000, 'agy-work')

    expect(readProviderProbeCache(storage, 'ws://host', 'antigravity', null, 2000, null)?.data.path).toBe('/bin/agy')
    expect(
      readProviderProbeCache(storage, 'ws://host', 'antigravity', '/opt/work/agy|~/.agy-work', 2000, 'agy-work')?.data.path,
    ).toBe('/opt/work/agy')
  })
})
