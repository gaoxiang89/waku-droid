import { describe, expect, test } from 'bun:test'
import type { ReportedCommand, SlashCommand } from './generated'
import { mergeComposerCommands } from './composer-autocomplete'

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
