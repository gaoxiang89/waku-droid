import { describe, expect, test } from 'bun:test'
import type { ProviderModel, ReportedCommand, SlashCommand } from './generated'
import {
  antigravityLocalCommandExecutesOnAccept,
  mergeComposerCommands,
  parseAntigravityLocalCommand,
  resolveProviderEffort,
  resolveProviderModel,
} from './composer-autocomplete'

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
