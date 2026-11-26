import type { ProviderKind, SlashCommand } from '@waku/client'

const DROID_CLIENT_COMMANDS = new Set(['model', 'reasoning', 'effort', 'autonomy', 'new', 'clear'])

/** These Droid commands require desktop bridges, not a provider prompt.
 * Filter only the builtins: user/project/skill templates still own their names. */
export function webComposerCommands(provider: ProviderKind, commands: SlashCommand[]): SlashCommand[] {
  return provider === 'droid'
    ? commands.filter((command) => !(DROID_CLIENT_COMMANDS.has(command.name)
      && command.scope === 'Builtin' && command.template == null))
    : commands
}

export function isUnsupportedWebCommand(provider: ProviderKind, prompt: string, commands: SlashCommand[]): boolean {
  const name = prompt.trim().match(/^\/(\S+)/u)?.[1]
  return provider === 'droid' && Boolean(name && DROID_CLIENT_COMMANDS.has(name)
    && !commands.some((command) => command.name === name
      && (command.scope !== 'Builtin' || command.template != null)))
}
