import { afterEach, describe, expect, mock, test } from 'bun:test';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { AgentSession, Command, DaemonSettings, PlanUsage, ResponsePayload, SequencedEvent, WakuClient } from '@waku/client';
import { reduceRuntimeEvent } from '@waku/client/event-reducer';
import { renderToStaticMarkup } from 'react-dom/server';

import { daemonKeys, hydrateSession } from './daemon-api';
import { beginTurn, createSession } from './mobile-runtime';

let activeDaemon: { activeProfile: { id: string }; client: WakuClient; phase: 'connected' };
mock.module('./daemon-context', () => ({ useDaemon: () => activeDaemon }));
mock.module('expo-crypto', () => ({ randomUUID: () => crypto.randomUUID() }));
mock.module('./composer-preferences-store', () => ({ persistentStorageSync: () => null }));

const { RuntimeProvider, useRuntime } = await import('./runtime-context');
const cleanups: Array<() => Promise<void>> = [];

afterEach(async () => {
  for (const cleanup of cleanups.splice(0)) await cleanup();
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

function fixture(options: {
  attached?: boolean;
  request?: (command: Command) => Promise<ResponsePayload | undefined>;
} = {}) {
  let nextId = 0;
  const clock = {
    nowSeconds: () => 100,
    nowMillis: () => 100_000,
    randomUUID: () => `id-${++nextId}`,
  };
  let sequence = 0;
  let history = createSession('project', 'codex', false, clock);
  function event(kind: string, payload: SequencedEvent['event']['payload']): SequencedEvent {
    return {
      sessionId: history.id,
      runtimeId: 'runtime',
      epoch: 'epoch',
      sequence: ++sequence,
      event: { kind, payload },
    };
  }
  history = beginTurn(history, 'Previous request', clock);
  history = reduceRuntimeEvent(history, event('turnStarted', null), clock).session;
  history = reduceRuntimeEvent(history, event('textDelta', 'Previous answer'), clock).session;
  history = reduceRuntimeEvent(history, event('turnFinished', { success: true }), clock).session;
  history = beginTurn(history, 'Current request', clock);
  history = reduceRuntimeEvent(history, event('turnStarted', null), clock).session;
  history = reduceRuntimeEvent(history, event('textDelta', 'Streaming response'), clock).session;
  const skeleton: AgentSession = {
    ...history,
    messages: [],
    turns: [],
    transcript_blocks: [],
    runtime_event_cursor: null,
  };
  const hydration = deferred<AgentSession | null>();
  const commands: Command[] = [];
  const settings: DaemonSettings = {
    provider_binary_overrides: {}, disabled_providers: [],
    computer_use_enabled: false, computer_use_allowed_apps: [],
    provider_accounts: ['acct-personal', 'acct-work'].map((id) => ({
      id, provider: 'codex', label: id, configDir: `/accounts/${id}`,
    })),
  };
  let listener: ((event: SequencedEvent) => void) | undefined;
  const client = {
    connected: true,
    request: async (command: Command): Promise<ResponsePayload> => {
      commands.push(command);
      const overridden = await options.request?.(command);
      if (overridden) return overridden;
      switch (command.type) {
        case 'hydrateSession':
          return { type: 'session', session: await hydration.promise };
        case 'attachSession':
          return { type: 'sessionRuntime', runtimeId: options.attached === false ? null : 'runtime', supportsSteer: true };
        case 'getSettings':
          return { type: 'settings', settings };
        case 'applyOptions':
          return { type: 'optionsApplied', applied: true };
        case 'loadTaskState':
          return {
            type: 'taskState',
            deletedProjectIds: [],
            defaultCwd: '/repo',
            projectlessRoot: null,
            projects: [{ id: 'project', name: 'Project', path: '/repo', created_at: 0 }],
            sessions: [history],
          };
        case 'probeProvider':
          return {
            type: 'providerProbe', version: null,
            probe: { provider: 'codex', installed: true, path: '/bin/codex', models: [], agent_presets: [] },
          };
        case 'start':
          return { type: 'started', supportsSteer: true };
        case 'saveTaskState':
          return { type: 'taskStateSaved', sessions: command.sessions };
        default:
          return { type: 'ack' };
      }
    },
    subscribe: (_sessionId: string, _runtimeId: string, next: typeof listener) => {
      listener = next;
      return () => { listener = undefined; };
    },
  } as unknown as WakuClient;
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: Infinity } } });
  const key = daemonKeys.session('profile', history.id);
  activeDaemon = { activeProfile: { id: 'profile' }, client, phase: 'connected' };
  let runtime!: ReturnType<typeof useRuntime>;
  function CaptureRuntime() {
    runtime = useRuntime();
    return null;
  }
  // Capture the real runtime callbacks without mounting a native view tree.
  // The tests exercise their refs, subscriptions and actual query-cache writes.
  renderToStaticMarkup(
    <QueryClientProvider client={queryClient}>
      <RuntimeProvider><CaptureRuntime /></RuntimeProvider>
    </QueryClientProvider>,
  );
  cleanups.push(async () => {
    await runtime.deleteSession(history.id);
    queryClient.clear();
  });
  return {
    history, skeleton, hydration, commands, client, queryClient, key, runtime, settings,
    emit(kind: string, payload: SequencedEvent['event']['payload']) {
      if (!listener) throw new Error('The runtime is not subscribed');
      listener(event(kind, payload));
    },
    current: () => queryClient.getQueryData<AgentSession>(key)!,
  };
}

describe('mobile runtime history', () => {
  test('creates a task with expanded provider text while retaining its slash command for display', async () => {
    const f = fixture({ attached: false });
    const session = await f.runtime.createTask('project', 'codex', false, '/deploy production', {}, '$deploy production');
    cleanups.push(() => f.runtime.deleteSession(session.id));
    expect(f.commands.find((command) => command.type === 'prompt'))
      .toMatchObject({ prompt: '$deploy production' });
    expect(session.messages[0]).toMatchObject({ content: '$deploy production', display_content: '/deploy production' });
  });

  test('keeps expanded command content separate from display text when steering', async () => {
    const f = fixture();
    f.queryClient.setQueryData(f.key, f.history);
    await f.runtime.attachSession(f.history);
    await f.runtime.steerPrompt(f.history, '/review changes', [], 'Review changes carefully');
    expect(f.commands.find((command) => command.type === 'steer'))
      .toEqual({ type: 'steer', prompt: 'Review changes carefully' });
    f.emit('steerAccepted', { message: 'Review changes carefully' });
    expect(f.current().messages.at(-1))
      .toMatchObject({ content: 'Review changes carefully', display_content: '/review changes' });
  });

  test('queues expanded command content without starting another provider turn', async () => {
    const f = fixture();
    f.queryClient.setQueryData(f.key, f.history);
    const session = await f.runtime.sendPrompt(f.history, '/review changes', [], 'Review changes carefully');
    expect(session.queued_messages?.at(-1))
      .toMatchObject({ content: 'Review changes carefully', display_content: '/review changes' });
    expect(f.commands.some((command) => command.type === 'prompt')).toBe(false);
  });

  test('sends a goal operation to a live runtime without adding a prompt', async () => {
    const f = fixture();
    f.queryClient.setQueryData(f.key, f.history);
    await f.runtime.sendGoalOperation(f.history, { kind: 'set', objective: null, status: 'paused', replace: false });
    expect(f.commands.at(-1)).toEqual({ type: 'goal', operation: { kind: 'set', objective: null, status: 'paused', replace: false } });
    expect(f.current().messages).toEqual(f.history.messages);
    expect(f.commands.some((command) => command.type === 'prompt')).toBe(false);
  });

  test('starts a provider for a fresh goal and applies its native updates', async () => {
    const f = fixture({ attached: false });
    const fresh = { ...f.history, messages: [], turns: [], transcript_blocks: [], status: 'idle' as const };
    f.queryClient.setQueryData(f.key, fresh);
    await f.runtime.sendGoalOperation(fresh, { kind: 'set', objective: 'Ship mobile', status: 'active', replace: false });
    expect(f.commands.find((command) => command.type === 'start'))
      .toMatchObject({ options: { cwd: '/repo', provider: 'codex' } });
    expect(f.commands.at(-1)).toEqual({ type: 'goal', operation: { kind: 'set', objective: 'Ship mobile', status: 'active', replace: false } });
    expect(f.commands.some((command) => command.type === 'prompt')).toBe(false);
    f.emit('goalUpdated', { objective: 'Ship mobile', status: 'active', tokensUsed: 0, timeUsedSeconds: 0 });
    expect(f.current().thread_goal?.objective).toBe('Ship mobile');
    expect(f.current().messages).toEqual([]);
  });

  test('preserves history across two desktop steers after attaching from a list placeholder', async () => {
    const f = fixture();
    const loading = f.queryClient.fetchQuery({
      queryKey: f.key,
      queryFn: () => hydrateSession(f.client, f.history.id),
    });
    const attaching = f.runtime.attachSession(f.skeleton);
    await Promise.resolve();
    const commandsBeforeHydration = f.commands.map((command) => command.type);
    f.hydration.resolve(f.history);
    await loading;
    await attaching;

    f.emit('textDelta', ' before steering');
    f.emit('availableCommands', []);
    // The runtime outlives the screen's query-cache entry.
    f.queryClient.removeQueries({ queryKey: f.key });
    f.emit('steerAccepted', { message: 'First desktop steer' });
    f.emit('steerAccepted', { message: 'Second desktop steer' });

    expect(f.current().messages.map((message) => message.content)).toEqual([
      'Previous request', 'Previous answer', 'Current request',
      'Streaming response before steering', 'First desktop steer', 'Second desktop steer',
    ]);
    expect(f.current().turns).toEqual(f.history.turns);
    expect(commandsBeforeHydration).toEqual(['hydrateSession']);
    expect(f.commands.filter((command) => command.type === 'hydrateSession')).toHaveLength(1);
  });

  test('keeps the latest streamed history when the query cache is evicted before two desktop steers', async () => {
    const f = fixture();
    f.queryClient.setQueryData(f.key, f.history);
    await f.runtime.attachSession(f.history);
    f.emit('textDelta', ' before steering');
    // An interactive event flushes pending streaming text immediately.
    f.emit('availableCommands', []);
    f.queryClient.removeQueries({ queryKey: f.key });
    f.emit('steerAccepted', { message: 'First desktop steer' });
    f.emit('steerAccepted', { message: 'Second desktop steer' });
    f.emit('textDelta', 'Continued response');
    f.emit('availableCommands', []);

    expect(f.current().messages.map((message) => message.content)).toEqual([
      'Previous request', 'Previous answer', 'Current request',
      'Streaming response before steering', 'First desktop steer', 'Second desktop steer',
      'Continued response',
    ]);
    expect(f.current().turns).toEqual(f.history.turns);
  });

  test('closes an idle runtime when switching provider accounts via updateSessionOptions', async () => {
    const f = fixture();
    const antigravitySession: AgentSession = {
      ...f.history,
      status: 'idle',
      turns: f.history.turns.map((turn) => ({ ...turn, status: 'completed' })),
      provider: 'antigravity',
      account_id: 'acct-personal',
      provider_cursor: { provider: 'antigravity', sessionId: 'personal-1' },
    };
    f.queryClient.setQueryData(f.key, antigravitySession);
    await f.runtime.attachSession(antigravitySession);
    await f.runtime.updateSessionOptions(antigravitySession.id, {
      accountId: 'acct-work',
      model: 'claude-sonnet-4-5',
    });

    expect(f.commands.some((command) => command.type === 'closeSession')).toBe(true);
    expect(f.commands.some((command) => command.type === 'applyOptions')).toBe(false);
    expect(f.current().account_id).toBe('acct-work');
    expect(f.current().model).toBe('claude-sonnet-4-5');
    expect(f.current().account_cursors).toEqual([{
      accountId: 'acct-personal',
      cursor: { provider: 'antigravity', sessionId: 'personal-1' },
    }]);
  });

  for (const status of ['working', 'waiting', 'connecting', 'background'] as const) {
    test(`rejects identity switches while ${status} without closing the driver`, async () => {
      const f = fixture();
      const session = { ...f.history, status, account_id: 'acct-personal' };
      f.queryClient.setQueryData(f.key, session);
      await f.runtime.attachSession(session);
      for (const change of [{ accountId: 'acct-work' }, { provider: 'claude' as const }]) {
        await expect(f.runtime.updateSessionOptions(session.id, change)).rejects.toThrow('Stop the current turn');
      }
      expect(f.commands.some((command) => command.type === 'closeSession')).toBe(false);
      expect(f.current().account_id).toBe('acct-personal');
      f.emit('availableCommands', [{ name: 'review', description: 'Still subscribed' }]);
      expect(f.current().available_commands).toHaveLength(1);
    });
  }

  test('rechecks latest session state after an async session load yields', async () => {
    const f = fixture();
    const idle = { ...f.history, status: 'idle' as const, account_id: 'acct-personal' };
    f.queryClient.setQueryData(f.key, idle);
    const changing = f.runtime.updateSessionOptions(idle.id, { accountId: 'acct-work' });
    f.queryClient.setQueryData(f.key, { ...idle, status: 'waiting' });
    await expect(changing).rejects.toThrow('Stop the current turn');
    expect(f.current().status).toBe('waiting');
    expect(f.current().account_id).toBe('acct-personal');
  });

  for (const operation of ['prompt', 'goal'] as const) {
    test(`preserves the startup subscription during overlapping ${operation} and account change`, async () => {
      const entered = deferred<void>();
      const started = deferred<ResponsePayload>();
      const f = fixture({ attached: false, request: async (command) => {
        if (command.type === 'start') { entered.resolve(); return started.promise; }
      } });
      const idle = { ...f.history, status: 'idle' as const, account_id: 'acct-personal' };
      f.queryClient.setQueryData(f.key, idle);
      const starting = operation === 'prompt'
        ? f.runtime.sendPrompt(idle, 'Continue')
        : f.runtime.sendGoalOperation(idle, { kind: 'clear' });
      await entered.promise;
      await expect(f.runtime.updateSessionOptions(idle.id, { accountId: 'acct-work' }))
        .rejects.toThrow('Stop the current turn');
      expect(f.commands.some((command) => command.type === 'closeSession')).toBe(false);
      started.resolve({ type: 'started', supportsSteer: true });
      await starting;
      f.emit('availableCommands', [{ name: 'review', description: 'Still subscribed' }]);
      expect(f.current().available_commands).toHaveLength(1);
      expect(f.current().account_id).toBe('acct-personal');
    });
  }

  test('guards identity before startup has even created a runtime entry', async () => {
    const f = fixture({ attached: false });
    const idle = { ...f.history, status: 'idle' as const, account_id: 'acct-personal' };
    f.queryClient.setQueryData(f.key, idle);
    const starting = f.runtime.sendPrompt(idle, 'Continue');
    await expect(f.runtime.updateSessionOptions(idle.id, { accountId: 'acct-work' }))
      .rejects.toThrow('Stop the current turn');
    await starting;
    expect(f.commands.filter((command) => command.type === 'start')).toHaveLength(1);
    expect(f.current().account_id).toBe('acct-personal');
  });

  test('allows model changes with unchanged identity while a turn is live', async () => {
    const f = fixture();
    f.queryClient.setQueryData(f.key, f.history);
    await f.runtime.attachSession(f.history);
    await f.runtime.updateSessionOptions(f.history.id, {
      provider: 'codex', accountId: null, model: 'new-model',
      reasoningEffort: 'high', serviceTier: null, contextWindow: null,
    });
    expect(f.current().model).toBe('new-model');
    expect(f.commands.find((command) => command.type === 'applyOptions'))
      .toMatchObject({ options: { model: 'new-model' } });
    expect(f.commands.some((command) => command.type === 'closeSession')).toBe(false);
  });

  test('unsupported live options do not close the driver or roll back streaming history', async () => {
    const applied = deferred<ResponsePayload>();
    const entered = deferred<void>();
    const f = fixture({ request: async (command) => {
      if (command.type === 'applyOptions') { entered.resolve(); return applied.promise; }
    } });
    f.queryClient.setQueryData(f.key, f.history);
    await f.runtime.attachSession(f.history);
    const changing = f.runtime.updateSessionOptions(f.history.id, { model: 'new-model' });
    await entered.promise;
    f.emit('textDelta', ' fresh text');
    f.emit('availableCommands', []);
    applied.resolve({ type: 'optionsApplied', applied: false });
    await expect(changing).rejects.toThrow('cannot change these options');
    expect(f.commands.some((command) => command.type === 'closeSession')).toBe(false);
    expect(f.current().messages.at(-1)?.content).toContain('fresh text');
    expect(f.current().model).toBe(f.history.model);
  });

  test('serializes overlapping submissions into one launch and one queued message', async () => {
    const f = fixture({ attached: false });
    const idle = { ...f.history, status: 'idle' as const };
    f.queryClient.setQueryData(f.key, idle);
    await Promise.all([
      f.runtime.sendPrompt(idle, 'First new turn'),
      f.runtime.sendPrompt(idle, 'Second new turn'),
    ]);
    expect(f.commands.filter((command) => command.type === 'start')).toHaveLength(1);
    expect(f.commands.filter((command) => command.type === 'prompt')).toHaveLength(1);
    expect(f.current().queued_messages?.[0]?.content).toBe('Second new turn');
  });

  test('sends an account handoff with clean display text, then recaps only unseen work on return', async () => {
    const f = fixture({ attached: false });
    const idle: AgentSession = {
      ...f.history, status: 'idle', account_id: 'acct-personal',
      provider_cursor: { provider: 'codex', threadId: 'personal-thread' },
      turns: f.history.turns.map((turn) => ({ ...turn, provider: 'codex', account_id: 'acct-personal', status: 'completed' })),
    };
    f.queryClient.setQueryData(f.key, idle);
    await f.runtime.updateSessionOptions(idle.id, { accountId: 'acct-work' });
    await f.runtime.sendPrompt(idle, 'Continue on work', [], 'Expanded work request');
    const firstPrompt = f.commands.find((command) => command.type === 'prompt');
    expect(firstPrompt).toMatchObject({ type: 'prompt', handoff: { record: { covered_turns: 2, resumed: false } } });
    if (firstPrompt?.type !== 'prompt') throw new Error('Missing prompt');
    expect(firstPrompt.prompt).toContain('Previous request');
    expect(firstPrompt.prompt).toContain('Expanded work request');
    expect(f.current().messages.at(-1)?.display_content).toBe('Continue on work');
    expect(f.current().messages.at(-2)?.role).toBe('system');
    f.emit('turnStarted', null);
    f.emit('turnFinished', { success: true });
    await f.runtime.updateSessionOptions(idle.id, { accountId: 'acct-personal' });
    await f.runtime.sendPrompt(idle, 'Back on personal');
    const prompts = f.commands.filter((command) => command.type === 'prompt');
    expect(prompts.at(-1)).toMatchObject({ handoff: { record: { covered_turns: 1, resumed: true } } });
    expect(prompts.at(-1)?.prompt).toContain('Continue on work');
    expect(prompts.at(-1)?.prompt).not.toContain('Previous request');
    expect(f.commands.filter((command) => command.type === 'start').at(-1))
      .toMatchObject({ options: { providerCursor: { provider: 'codex', threadId: 'personal-thread' } } });
  });

  for (const change of ['deleted', 'configDir', 'binaryOverride'] as const) {
    test(`never writes ${change} named runtime quota into Default`, async () => {
      const f = fixture({ attached: false });
      const idle = { ...f.history, status: 'idle' as const, account_id: 'acct-personal' };
      f.queryClient.setQueryData(f.key, idle);
      await f.runtime.sendPrompt(idle, 'Continue');
      const settings = structuredClone(f.settings);
      if (change === 'deleted') settings.provider_accounts = [];
      else settings.provider_accounts![0]![change] = '/changed';
      f.queryClient.setQueryData(daemonKeys.settings('profile'), settings);
      f.emit('planUsageUpdated', { planLabel: 'Named plan', windows: [] });
      expect(f.queryClient.getQueryData(daemonKeys.planUsage('profile', 'codex', null))).toBeUndefined();
      expect(f.queryClient.getQueryData(daemonKeys.planUsage('profile', 'codex', 'acct-personal'))).toBeUndefined();
    });
  }

  test('attributes quota to launch identity rather than a changed session projection', async () => {
    const f = fixture({ attached: false });
    const idle = { ...f.history, status: 'idle' as const, account_id: 'acct-personal' };
    f.queryClient.setQueryData(f.key, idle);
    await f.runtime.sendPrompt(idle, 'Continue');
    f.queryClient.setQueryData(f.key, { ...f.current(), account_id: 'acct-work' });
    const usage = { planLabel: 'Personal plan', windows: [] };
    f.emit('planUsageUpdated', usage);
    expect(f.queryClient.getQueryData<PlanUsage>(daemonKeys.planUsage('profile', 'codex', 'acct-personal'))).toEqual(usage);
    expect(f.queryClient.getQueryData<PlanUsage>(daemonKeys.planUsage('profile', 'codex', 'acct-work'))).toBeUndefined();
  });
});
