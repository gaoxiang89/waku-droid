import { describe, expect, test } from 'bun:test'
import { createSession } from './daemon-api'
import { mergeSessionSummary } from './runtime-context'

describe('mergeSessionSummary', () => {
  test('preserves context_window and account fields while keeping local transcript messages', () => {
    const previous = {
      ...createSession('project', 'antigravity', false),
      messages: [
        {
          id: 'msg-1',
          turn_id: 'turn-1',
          role: 'user' as const,
          content: 'Hello',
          attachments: [],
          created_at: 100,
          streaming: false,
        },
      ],
      context_window: null,
    }
    const summary = {
      ...createSession('project', 'antigravity', false),
      id: previous.id,
      account_id: 'acct-work',
      context_window: '1m',
      status: 'working' as const,
      updated_at: 200,
      messages: [],
    }

    const merged = mergeSessionSummary(previous, summary)
    expect(merged.context_window).toBe('1m')
    expect(merged.account_id).toBe('acct-work')
    expect(merged.status).toBe('working')
    expect(merged.messages).toEqual(previous.messages)
    expect(merged.state_version).toBe(0)

    const bumped = mergeSessionSummary(previous, { ...summary, state_version: 7 })
    expect(bumped.state_version).toBe(7)
  })
})
