import { describe, expect, test } from 'bun:test'
import type { AgentSession, ProviderKind, ProviderResumeCursor } from './generated'
import {
  responseForkTurnCount,
  supportsConversationFork,
  supportsConversationRollback,
  userMessageRewindTurnCount,
} from './transcript-presentation'

const SUPPORTED_PROVIDERS: ProviderKind[] = [
  'amp',
  'antigravity',
  'claude',
  'codex',
  'cursor',
  'deepSeek',
  'openCode',
  'grok',
  'ohMyPi',
  'pi',
]

const UNSUPPORTED_PROVIDERS: ProviderKind[] = ['droid', 'fx', 'kimi']

function cursorFor(provider: ProviderKind): ProviderResumeCursor {
  switch (provider) {
    case 'amp':
      return { provider: 'amp', threadId: 't1' }
    case 'antigravity':
      return { provider: 'antigravity', sessionId: 's1' }
    case 'claude':
      return { provider: 'claude', sessionId: 's1' }
    case 'codex':
      return { provider: 'codex', threadId: 't1' }
    case 'cursor':
      return { provider: 'cursor', sessionId: 's1' }
    case 'deepSeek':
      return { provider: 'deepSeek', sessionId: 's1' }
    case 'droid':
      return { provider: 'droid', sessionId: 's1' }
    case 'fx':
      return { provider: 'fx', sessionId: 's1' }
    case 'openCode':
      return { provider: 'openCode', sessionId: 's1' }
    case 'grok':
      return { provider: 'grok', sessionId: 's1' }
    case 'kimi':
      return { provider: 'kimi', sessionId: 's1' }
    case 'ohMyPi':
      return { provider: 'ohMyPi', sessionId: 's1' }
    case 'pi':
      return { provider: 'pi', sessionId: 's1' }
  }
}

function sampleSession(provider: ProviderKind): AgentSession {
  return {
    id: 'session-1',
    title: 'Task',
    pinned: false,
    archived: false,
    project_id: 'project-1',
    provider,
    runtime_mode: 'fullAccess',
    status: 'idle',
    created_at: 10,
    updated_at: 200,
    provider_cursor: cursorFor(provider),
    turns: [{
      id: 'turn-1',
      turn_count: 1,
      status: 'completed',
      provider_turn_started: true,
      started_at: 100,
      completed_at: 200,
      checkpoint: null,
    }],
    queued_messages: [],
    transcript_blocks: [],
    messages: [
      {
        id: 'm1',
        turn_id: 'turn-1',
        role: 'user',
        content: 'Build it',
        created_at: 100,
        streaming: false,
      },
      {
        id: 'm2',
        turn_id: 'turn-1',
        role: 'assistant',
        content: 'Done.',
        created_at: 200,
        streaming: false,
      },
    ],
  }
}

describe('provider conversation rollback & fork gating', () => {
  test('matches waku-protocol capability truth tables for every provider', () => {
    for (const provider of SUPPORTED_PROVIDERS) {
      expect(supportsConversationRollback(provider)).toBe(true)
      expect(supportsConversationFork(provider)).toBe(true)
    }
    for (const provider of UNSUPPORTED_PROVIDERS) {
      expect(supportsConversationRollback(provider)).toBe(false)
      expect(supportsConversationFork(provider)).toBe(false)
    }
  })

  test('hides Rewind and Fork for Droid, Fx, and Kimi while keeping them for supported providers', () => {
    const footer = { content: 'Done.', timestamp: 200 }
    for (const provider of UNSUPPORTED_PROVIDERS) {
      const session = sampleSession(provider)
      expect(userMessageRewindTurnCount(session, session.messages[0]!, new Set([0]))).toBeNull()
      expect(responseForkTurnCount(session, session.messages[1]!, footer, session.turns[0])).toBeNull()
    }
    for (const provider of ['claude', 'codex', 'antigravity'] as const) {
      const session = sampleSession(provider)
      expect(userMessageRewindTurnCount(session, session.messages[0]!, new Set([0]))).toBe(1)
      expect(responseForkTurnCount(session, session.messages[1]!, footer, session.turns[0])).toBe(1)
    }
  })
})
