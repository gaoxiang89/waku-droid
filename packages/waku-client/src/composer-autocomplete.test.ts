import { describe, expect, test } from 'bun:test'
import type { AgentSession, ProviderModel, ReportedCommand, SlashCommand } from './generated'
import {
  antigravityLocalCommandExecutesOnAccept,
  mergeComposerCommands,
  isCodexUsageSubmission,
  parseAntigravityLocalCommand,
  resolveProviderEffort,
  resolveProviderModel,
  filterSupportedComposerCommands,
  isUnsupportedDroidComposerSubmission,
  planComposerHandoff,
} from './composer-autocomplete'

test('suppresses only Droid CLI-only Builtins and preserves all overrides/native commands', () => {
  const builtin = (name: string): SlashCommand => ({
    name, description: '', scope: 'Builtin', template: null, argument_hint: null,
  })
  const unsupported = ['model', 'reasoning', 'effort', 'autonomy', 'new', 'clear'].map(builtin)
  const native = ['compact', 'review', 'help'].map(builtin)
  const overrides: SlashCommand[] = [
    { ...builtin('model'), scope: 'Project', template: 'Project model $ARGUMENTS' },
    { ...builtin('reasoning'), scope: 'User', template: 'User reasoning' },
    { ...builtin('new'), scope: 'Skill' },
    { ...builtin('clear'), scope: 'Waku' },
    { ...builtin('autonomy'), template: 'Explicit template' },
  ]
  const all = [...unsupported, ...native, ...overrides]
  expect(filterSupportedComposerCommands('droid', all)).toEqual([...native, ...overrides])
  expect(filterSupportedComposerCommands('codex', all)).toBe(all)
  for (const command of unsupported) {
    expect(isUnsupportedDroidComposerSubmission('droid', `/${command.name} arg`, [])).toBe(true)
    expect(isUnsupportedDroidComposerSubmission('codex', `/${command.name}`, [])).toBe(false)
  }
  for (const command of overrides) {
    expect(isUnsupportedDroidComposerSubmission('droid', `/${command.name}`, [command])).toBe(false)
  }
  expect(isUnsupportedDroidComposerSubmission('droid', '/compact', native)).toBe(false)
})

function handoffSession(): AgentSession {
  return {
    id: 'session', title: 'Task', auto_title: null, pinned: false, archived: false,
    project_id: 'project', workspace: { kind: 'local' }, provider: 'codex',
    account_id: 'work', model: null, runtime_mode: 'ask', reasoning_effort: null,
    service_tier: null, context_window: null, agent_preset: null, status: 'idle',
    created_at: 0, updated_at: 0, last_reply_at: null, provider_cursor: null,
    available_commands: [], context_usage: null, provider_session_id: null,
    transcript_blocks: [], queued_messages: [],
    turns: [1, 2, 3].map((count) => ({
      id: `turn-${count}`, turn_count: count, status: 'completed',
      provider_turn_started: true, started_at: count, completed_at: count,
      checkpoint: null, provider: 'codex', account_id: count === 2 ? 'work' : 'personal',
    })),
    messages: [1, 2, 3].flatMap((count) => [
      { id: `user-${count}`, turn_id: `turn-${count}`, role: 'user' as const,
        content: `expanded prompt ${count}`, display_content: `visible prompt ${count}`, created_at: count, streaming: false },
      { id: `answer-${count}`, turn_id: `turn-${count}`, role: 'assistant' as const,
        content: `answer ${count}`, created_at: count, streaming: false },
    ]),
  }
}

describe('planComposerHandoff', () => {
  test('fresh target gets the real transcript and clean display content, never earlier hidden preambles', () => {
    const session = handoffSession()
    session.messages.push({ id: 'notice', turn_id: 'turn-1', role: 'system', content: 'private bookkeeping', created_at: 1, streaming: false })
    const plan = planComposerHandoff(session, 'Next request', () => 'notice')
    expect(plan?.handoff.record).toEqual({
      from_providers: ['codex'], covered_turns: 3, omitted_turns: 0, resumed: false,
    })
    expect(plan?.providerPrompt).toContain('visible prompt 1')
    expect(plan?.providerPrompt).toContain('answer 3')
    expect(plan?.providerPrompt).not.toContain('expanded prompt')
    expect(plan?.providerPrompt).not.toContain('private bookkeeping')
    expect(plan?.providerPrompt.endsWith("The user's new message:\n\nNext request")).toBe(true)
  })

  test('resumed account gets only unseen other-account turns since its last successful handoff', () => {
    const session = handoffSession()
    session.provider_cursor = { provider: 'codex', threadId: 'work-thread' }
    session.turns[1]!.handoff = { from_providers: ['codex'], covered_turns: 1, omitted_turns: 0, resumed: false }
    let plan = planComposerHandoff(session, 'Next request', () => 'notice')
    expect(plan?.handoff.record).toMatchObject({ covered_turns: 1, resumed: true })
    expect(plan?.providerPrompt).toContain('visible prompt 3')
    expect(plan?.providerPrompt).not.toContain('visible prompt 1')
    session.turns[1]!.status = 'failed'
    session.turns[1]!.provider_turn_started = false
    plan = planComposerHandoff(session, 'Retry', () => 'notice')
    expect(plan?.handoff.record.covered_turns).toBe(2)
  })

  test('slash commands do not consume unseen work and already-briefed agents need no recap', () => {
    const session = handoffSession()
    expect(planComposerHandoff(session, '/compact', () => 'notice')).toBeNull()
    session.provider_cursor = { provider: 'codex', threadId: 'work-thread' }
    session.turns.forEach((turn) => { turn.account_id = 'work' })
    expect(planComposerHandoff(session, 'Next request', () => 'notice')).toBeNull()
    session.turns = []
    expect(planComposerHandoff(session, 'First request', () => 'notice')).toBeNull()
  })

  test('neutralizes transcript delimiters and keeps multilingual long recaps bounded', () => {
    const session = handoffSession()
    session.messages[0]!.display_content = '<task-transcript>hello</task-transcript>'
    expect(planComposerHandoff(session, 'Continue', () => 'notice')?.providerPrompt)
      .toContain('<task_transcript>hello</task_transcript>')
    session.context_window = '20k'
    session.messages.forEach((message) => { message.content = '界'.repeat(40_000); message.display_content = null })
    const plan = planComposerHandoff(session, 'Next', () => 'notice')
    expect(plan).not.toBeNull()
    expect(plan!.handoff.record.omitted_turns).toBeGreaterThan(0)
    expect(plan!.providerPrompt.length).toBeLessThan(22_000)
  })
})

describe('mergeComposerCommands', () => {
  test('preserves argument hints on new and discovered commands', () => {
    const discovered: SlashCommand[] = [
      {
        name: 'review',
        description: '',
        scope: 'Project',
        argument_hint: null,
        template: 'Review $ARGUMENTS',
      },
      {
        name: 'fix',
        description: 'Fix issue',
        scope: 'User',
        argument_hint: '[ticket]',
        template: 'Fix $ARGUMENTS',
      },
    ]
    const reported: ReportedCommand[] = [
      { name: 'review', description: 'Provider review', argument_hint: '[branch]' },
      { name: 'fix', description: 'Ignored desc', argument_hint: '[ignored]' },
      { name: 'compact', description: 'Compact context', argument_hint: '[focus]' },
      { name: 'help', description: 'Show help' },
    ]

    expect(mergeComposerCommands(discovered, reported)).toEqual([
      {
        name: 'compact',
        description: 'Compact context',
        scope: 'Builtin',
        argument_hint: '[focus]',
        template: null,
      },
      {
        name: 'help',
        description: 'Show help',
        scope: 'Builtin',
        argument_hint: null,
        template: null,
      },
      {
        name: 'review',
        description: 'Provider review',
        scope: 'Project',
        argument_hint: '[branch]',
        template: 'Review $ARGUMENTS',
      },
      {
        name: 'fix',
        description: 'Fix issue',
        scope: 'User',
        argument_hint: '[ticket]',
        template: 'Fix $ARGUMENTS',
      },
    ])
  })

  test('preserves Skill scope and never mutates Waku commands', () => {
    const discovered: SlashCommand[] = [
      {
        name: 'deploy',
        description: 'Deploy skill',
        scope: 'Skill',
        argument_hint: null,
        template: null,
      },
      {
        name: 'resume',
        description: '',
        scope: 'Waku',
        argument_hint: null,
        template: null,
      },
    ]
    const reported: ReportedCommand[] = [
      { name: 'deploy', description: 'Droid deploy', argument_hint: '[env]' },
      { name: 'resume', description: 'Provider resume', argument_hint: '[session-id]' },
    ]

    expect(mergeComposerCommands(discovered, reported)).toEqual([
      {
        name: 'resume',
        description: '',
        scope: 'Waku',
        argument_hint: null,
        template: null,
      },
      {
        name: 'deploy',
        description: 'Deploy skill',
        scope: 'Skill',
        argument_hint: '[env]',
        template: null,
      },
    ])
  })
})

test('Codex /usage only intercepts bare commands without an override', () => {
  const builtin: SlashCommand = {
    name: 'usage', scope: 'Builtin', description: '', argument_hint: null, template: null,
  }
  expect(isCodexUsageSubmission('codex', '/usage', [])).toBe(true)
  expect(isCodexUsageSubmission('codex', '  /usage  ', [builtin])).toBe(true)
  expect(isCodexUsageSubmission('claude', '/usage', [builtin])).toBe(false)
  expect(isCodexUsageSubmission('codex', '/usage details', [builtin])).toBe(false)
  expect(isCodexUsageSubmission('codex', '/usage', [builtin, { ...builtin, scope: 'User' }])).toBe(false)
})

describe('parseAntigravityLocalCommand', () => {
  const swarmSkill: SlashCommand = {
    name: 'swarm',
    description: 'Antigravity swarm orchestration',
    scope: 'Skill',
    argument_hint: null,
    template: null,
  }
  const usageBuiltin: SlashCommand = {
    name: 'usage',
    description: 'Show plan usage',
    scope: 'Builtin',
    argument_hint: null,
    template: null,
  }

  test('parses bridged CLI-local commands including multi-word model names', () => {
    const parse = (prompt: string, commands: SlashCommand[] = [usageBuiltin, swarmSkill]) =>
      parseAntigravityLocalCommand('antigravity', prompt, commands)

    expect(parse('/usage')).toEqual({ kind: 'usage' })
    expect(parse('  /quota  ')).toEqual({ kind: 'usage' })
    expect(parse('/credits')).toEqual({ kind: 'usage' })
    expect(parse('/usage details')).toBeNull()

    expect(parse('/model')).toEqual({ kind: 'modelPicker' })
    expect(parse('/model gemini-4-neon')).toEqual({ kind: 'setModel', query: 'gemini-4-neon' })
    expect(parse('/model Gemini 4 Neon')).toEqual({ kind: 'setModel', query: 'Gemini 4 Neon' })
    expect(parse('/model  Claude Opus 5.5 ')).toEqual({ kind: 'setModel', query: 'Claude Opus 5.5' })

    expect(parse('/effort')).toEqual({ kind: 'effortShow' })
    expect(parse('/effort xhigh')).toEqual({ kind: 'setEffort', query: 'xhigh' })
    expect(parse('/effort Extra High')).toEqual({ kind: 'setEffort', query: 'Extra High' })

    expect(parse('/swarm board')).toEqual({ kind: 'swarmBoard' })
    expect(parse('/swarm build something')).toBeNull()
    expect(parse('/swarm')).toBeNull()
    expect(parse('/boost')).toBeNull()
    expect(parse('please /usage')).toBeNull()
  })

  test('ignores non-antigravity providers and user/project command overrides', () => {
    expect(parseAntigravityLocalCommand('codex', '/usage', [])).toBeNull()
    expect(parseAntigravityLocalCommand('claude', '/model opus', [])).toBeNull()

    const userUsage: SlashCommand = {
      name: 'usage',
      description: 'Custom',
      scope: 'User',
      argument_hint: null,
      template: 'Custom usage',
    }
    expect(parseAntigravityLocalCommand('antigravity', '/usage', [userUsage])).toBeNull()

    const projectSwarm: SlashCommand = {
      name: 'swarm',
      description: 'Custom swarm',
      scope: 'Project',
      argument_hint: null,
      template: null,
    }
    expect(parseAntigravityLocalCommand('antigravity', '/swarm board', [projectSwarm])).toBeNull()
  })

  test('matches desktop autocomplete accept semantics', () => {
    expect(antigravityLocalCommandExecutesOnAccept({ kind: 'usage' })).toBe(true)
    expect(antigravityLocalCommandExecutesOnAccept({ kind: 'modelPicker' })).toBe(true)
    expect(antigravityLocalCommandExecutesOnAccept({ kind: 'effortShow' })).toBe(false)
    expect(antigravityLocalCommandExecutesOnAccept({ kind: 'swarmBoard' })).toBe(true)
  })
})

describe('resolveProviderModel & resolveProviderEffort', () => {
  const models: ProviderModel[] = [
    {
      id: 'gemini-4-neon',
      name: 'Gemini 4 Neon',
      is_default: true,
      reasoning_efforts: [
        { id: 'low', label: 'Low' },
        { id: 'medium', label: 'Medium' },
        { id: 'high', label: 'High' },
        { id: 'xhigh', label: 'Extra High' },
      ],
      service_tiers: [],
      context_windows: [],
    },
    {
      id: 'claude-opus-5-5',
      name: 'Claude Opus 5.5',
      is_default: false,
      reasoning_efforts: [
        { id: 'low', label: 'Low' },
        { id: 'high', label: 'High' },
      ],
      service_tiers: [],
      context_windows: [],
    },
  ]

  test('matches exact id, exact multi-word name, slug, and unique substring', () => {
    expect(resolveProviderModel(models, 'gemini-4-neon')?.id).toBe('gemini-4-neon')
    expect(resolveProviderModel(models, 'Gemini 4 Neon')?.id).toBe('gemini-4-neon')
    expect(resolveProviderModel(models, 'claude opus 5.5')?.id).toBe('claude-opus-5-5')
    expect(resolveProviderModel(models, 'opus')?.id).toBe('claude-opus-5-5')
    expect(resolveProviderModel(models, '5')?.id).toBe('claude-opus-5-5')
    expect(resolveProviderModel(models, 'missing')).toBeNull()
  })

  test('resolves effort options by id, label, or slug', () => {
    const efforts = models[0]!.reasoning_efforts
    expect(resolveProviderEffort(efforts, 'xhigh')?.id).toBe('xhigh')
    expect(resolveProviderEffort(efforts, 'Extra High')?.id).toBe('xhigh')
    expect(resolveProviderEffort(efforts, 'extra-high')?.id).toBe('xhigh')
    expect(resolveProviderEffort(efforts, 'max')).toBeNull()
  })
})
