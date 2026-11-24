import { useQuery, useQueryClient } from '@tanstack/react-query';
import {
  resolveProviderAccount,
  type GoalOperation,
  type ProviderKind,
  type ProviderModel,
  type ProviderSessionSummary,
  type RuntimeMode,
  type SlashCommand,
  type ThreadGoal,
} from '@waku/client';
import {
  isFastModeToggleSubmission,
  isResumeSubmission,
  parseAntigravityLocalCommand,
  parseGoalSubmission,
  resolveProviderEffort,
  resolveProviderModel,
  toggledFastServiceTier,
} from '@waku/client/composer-autocomplete';
import { fuzzyScore } from '@waku/client/fuzzy-search';
import * as Crypto from 'expo-crypto';
import * as Haptics from 'expo-haptics';
import { router } from 'expo-router';
import { useEffect, useMemo, useState } from 'react';
import { ActivityIndicator, Alert, FlatList, StyleSheet, Text, TextInput, View, useWindowDimensions } from 'react-native';

import { Sheet, SheetRow } from './sheet';
import { NativeTint, Radius } from '@/constants/theme';
import { useDaemonSettings } from '@/hooks/use-daemon-data';
import { useTheme } from '@/hooks/use-theme';
import {
  createProject,
  daemonKeys,
  fetchPlanUsage,
  listProviderSessions,
  loadProviderSessionHistory,
  loadTaskState,
  persistProject,
  persistSession,
  providerSessionKey,
} from '@/lib/daemon-api';
import { useDaemon } from '@/lib/daemon-context';
import { createResumedSession } from '@/lib/mobile-runtime';
import { providerLabel } from '@/lib/session-presentation';

/** Desktop's local commands must never leak through as provider prompts. */
export function useComposerLocalCommands({
  provider,
  accountId = null,
  models,
  model,
  providerVersion = null,
  reasoningEffort,
  serviceTier,
  runtimeMode,
  goal,
  canSwitchModel = true,
  contextKey,
  onOpenModelPicker,
  onSelectModel,
  onReasoningEffort,
  onServiceTier,
  onGoal,
  onClear,
}: {
  provider: ProviderKind | null;
  accountId?: string | null;
  models?: ProviderModel[];
  model: ProviderModel | undefined;
  providerVersion?: string | null;
  reasoningEffort?: string | null;
  serviceTier: string | null | undefined;
  runtimeMode: RuntimeMode;
  goal?: ThreadGoal | null;
  canSwitchModel?: boolean;
  contextKey: string;
  onOpenModelPicker?: () => void;
  onSelectModel?: (model: ProviderModel) => void | Promise<void>;
  onReasoningEffort?: (effort: string) => void | Promise<void>;
  onServiceTier: (tier: string) => void | Promise<void>;
  onGoal: (operation: GoalOperation) => Promise<void>;
  onClear: () => void;
}) {
  const daemon = useDaemon();
  const [resumeOpen, setResumeOpen] = useState(false);
  const [usageOpen, setUsageOpen] = useState(false);
  const [goalDialog, setGoalDialog] = useState<{ prefill: string | null; replace: boolean } | null>(null);
  useEffect(() => {
    setResumeOpen(false);
    setUsageOpen(false);
    setGoalDialog(null);
  }, [accountId, contextKey, daemon.activeProfile?.id, provider]);

  async function execute(prompt: string, commands: SlashCommand[]): Promise<boolean> {
    if (!provider) return false;
    const antigravity = parseAntigravityLocalCommand(provider, prompt, commands);
    if (antigravity) {
      switch (antigravity.kind) {
        case 'usage':
          setUsageOpen(true);
          break;
        case 'modelPicker':
          if (!canSwitchModel) throw new Error('Stop the current turn before switching models');
          onOpenModelPicker?.();
          break;
        case 'setModel': {
          if (!canSwitchModel) throw new Error('Stop the current turn before switching models');
          const matched = resolveProviderModel(models ?? [], antigravity.query);
          if (!matched) throw new Error(`Unknown model: ${antigravity.query}`);
          await onSelectModel?.(matched);
          void Haptics.selectionAsync();
          Alert.alert('Model switched', `Switched model to ${matched.name}`);
          break;
        }
        case 'effortShow': {
          const efforts = model?.reasoning_efforts ?? [];
          if (!efforts.length) {
            Alert.alert('Reasoning effort', 'This model does not support reasoning effort selection.');
          } else {
            const currentId = efforts.some((option) => option.id === reasoningEffort)
              ? reasoningEffort
              : (model?.default_reasoning_effort ?? efforts[0]!.id);
            const current = efforts.find((option) => option.id === currentId) ?? efforts[0]!;
            const options = efforts.map((option) => option.label).join(', ');
            Alert.alert('Reasoning effort', `Reasoning effort: ${current.label} (available: ${options})`);
          }
          break;
        }
        case 'setEffort': {
          const efforts = model?.reasoning_efforts ?? [];
          if (!efforts.length) {
            throw new Error('This model does not support reasoning effort selection');
          }
          const matched = resolveProviderEffort(efforts, antigravity.query);
          if (!matched) {
            const options = efforts.map((option) => option.label).join(', ');
            throw new Error(`Unknown reasoning effort "${antigravity.query}" (available: ${options})`);
          }
          await onReasoningEffort?.(matched.id);
          void Haptics.selectionAsync();
          Alert.alert('Reasoning effort', `Reasoning effort set to ${matched.label}`);
          break;
        }
        case 'swarmBoard':
          Alert.alert('Swarm board', 'Swarm board is only available in Waku Desktop.');
          break;
      }
      onClear();
      return true;
    }

    if (isResumeSubmission(prompt)) {
      setResumeOpen(true);
    } else if (isFastModeToggleSubmission(provider, prompt, commands)) {
      const tier = toggledFastServiceTier(serviceTier, model?.service_tiers ?? []);
      if (!tier) throw new Error('Fast mode is unavailable for this model');
      await onServiceTier(tier);
    } else {
      const command = parseGoalSubmission(provider, prompt, commands);
      if (!command) return false;
      switch (command.kind) {
        case 'show':
        case 'edit':
          setGoalDialog({ prefill: null, replace: false });
          break;
        case 'pause':
        case 'resume':
          await onGoal({ kind: 'set', objective: null, status: command.kind === 'pause' ? 'paused' : 'active', replace: false });
          break;
        case 'clear':
          await onGoal({ kind: 'clear' });
          break;
        case 'set':
          if (goal && goal.status !== 'complete' && goal.status !== 'budgetLimited') {
            setGoalDialog({ prefill: command.objective, replace: true });
          } else {
            await onGoal({ kind: 'set', objective: command.objective, status: 'active', replace: Boolean(goal) });
          }
          break;
      }
    }
    onClear();
    return true;
  }

  return {
    execute,
    sheets: (
      <>
        {resumeOpen && provider && (
          <ResumeSessionSheet
            accountId={accountId}
            provider={provider}
            runtimeMode={runtimeMode}
            onDismiss={() => setResumeOpen(false)}
          />
        )}
        {usageOpen && provider && (
          <PlanUsageSheet
            accountId={accountId}
            provider={provider}
            providerVersion={providerVersion}
            onDismiss={() => setUsageOpen(false)}
          />
        )}
        {goalDialog && (
          <GoalCommandSheet
            goal={goal ?? null}
            prefill={goalDialog.prefill}
            replace={goalDialog.replace}
            onRun={onGoal}
            onDismiss={() => setGoalDialog(null)}
          />
        )}
      </>
    ),
  };
}

function ResumeSessionSheet({ provider, accountId = null, runtimeMode, onDismiss }: {
  provider: ProviderKind;
  accountId?: string | null;
  runtimeMode: RuntimeMode;
  onDismiss: () => void;
}) {
  const theme = useTheme();
  const daemon = useDaemon();
  const queryClient = useQueryClient();
  const { height } = useWindowDimensions();
  const profileId = daemon.activeProfile?.id ?? 'disconnected';
  const [search, setSearch] = useState('');
  const [resuming, setResuming] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const sessions = useQuery({
    queryKey: daemonKeys.providerSessions(profileId, provider, accountId),
    queryFn: () => listProviderSessions(daemon.client!, provider, accountId),
    enabled: daemon.phase === 'connected' && Boolean(daemon.client),
    staleTime: 15_000,
  });
  const rows = useMemo(() => (sessions.data ?? []).filter((item) =>
    fuzzyScore(search, `${item.title} ${item.cwd}`) !== null), [search, sessions.data]);

  async function resume(summary: ProviderSessionSummary) {
    const client = daemon.client;
    if (!client || daemon.phase !== 'connected' || resuming) return;
    setResuming(providerSessionKey(summary.cursor));
    setError(null);
    try {
      const state = await loadTaskState(client);
      let session = state.sessions.find((item) => item.provider_cursor
        && providerSessionKey(item.provider_cursor) === providerSessionKey(summary.cursor)
        && (item.account_id ?? null) === (accountId ?? null));
      if (!session) {
        const history = await loadProviderSessionHistory(client, summary, accountId);
        let project = state.projects.find((item) => item.path === summary.cwd);
        if (!project) project = (await persistProject(client, createProject(summary.cwd, Crypto.randomUUID()))).project;
        session = await persistSession(client, {
          ...createResumedSession(
            project.id, summary, history, runtimeMode,
            { nowSeconds: () => Math.floor(Date.now() / 1_000), randomUUID: Crypto.randomUUID },
          ),
          account_id: accountId ?? null,
        });
        queryClient.setQueryData(daemonKeys.session(profileId, session.id), session);
      }
      await queryClient.invalidateQueries({ queryKey: daemonKeys.taskState(profileId) });
      onDismiss();
      router.push({ pathname: '/session/[id]', params: { id: session.id } });
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setResuming(null);
    }
  }

  return (
    <Sheet visible onDismiss={onDismiss} scrollable={false} title={`Resume ${providerLabel(provider)} session`}>
      <TextInput
        accessibilityLabel="Search provider sessions"
        placeholder="Search sessions…"
        placeholderTextColor={theme.textTertiary}
        selectionColor={NativeTint}
        style={[styles.input, { color: theme.text, backgroundColor: theme.inset }]}
        value={search}
        onChangeText={setSearch}
      />
      {(error || sessions.error) && <Text style={[styles.note, { color: theme.danger }]}>{error ?? String(sessions.error)}</Text>}
      {sessions.isPending ? <ActivityIndicator style={styles.loading} color={theme.textTertiary} /> : (
        <FlatList
          data={rows}
          extraData={resuming}
          initialNumToRender={8}
          keyExtractor={(item) => providerSessionKey(item.cursor)}
          keyboardShouldPersistTaps="handled"
          style={{ maxHeight: height * 0.45 }}
          ListEmptyComponent={<Text style={[styles.note, { color: theme.textTertiary }]}>No matching sessions</Text>}
          renderItem={({ item }) => (
            <SheetRow
              label={item.title || 'Untitled session'}
              description={item.cwd}
              disabled={Boolean(resuming)}
              leading={resuming === providerSessionKey(item.cursor) ? <ActivityIndicator size="small" /> : undefined}
              onPress={() => void resume(item)}
            />
          )}
        />
      )}
      {sessions.error && <SheetRow label="Retry" onPress={() => { void sessions.refetch(); }} />}
    </Sheet>
  );
}

function GoalCommandSheet({ goal, prefill, replace, onRun, onDismiss }: {
  goal: ThreadGoal | null;
  prefill: string | null;
  replace: boolean;
  onRun: (operation: GoalOperation) => Promise<void>;
  onDismiss: () => void;
}) {
  const theme = useTheme();
  const [objective, setObjective] = useState(prefill ?? goal?.objective ?? '');
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  async function run(operation: GoalOperation) {
    if (pending) return;
    setPending(true);
    setError(null);
    try {
      await onRun(operation);
      onDismiss();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setPending(false);
    }
  }
  return (
    <Sheet visible onDismiss={onDismiss} title="Goal">
      {goal && <Text style={[styles.note, { color: theme.textSecondary }]}>Status: {{
        active: 'Active', paused: 'Paused', blocked: 'Stalled', usageLimited: 'Usage limit reached',
        budgetLimited: 'Budget reached', complete: 'Complete',
      }[goal.status]}</Text>}
      <TextInput
        accessibilityLabel="Goal objective"
        multiline
        editable={!pending}
        placeholder="What should the agent keep working on?"
        placeholderTextColor={theme.textTertiary}
        selectionColor={NativeTint}
        style={[styles.input, styles.objective, { color: theme.text, backgroundColor: theme.inset }]}
        value={objective}
        onChangeText={setObjective}
      />
      {replace && goal && <Text style={[styles.note, { color: theme.textSecondary }]}>This replaces the current goal: {goal.objective}</Text>}
      {error && <Text style={[styles.note, { color: theme.danger }]}>{error}</Text>}
      <SheetRow
        label={goal ? replace ? 'Replace goal' : 'Save goal' : 'Set goal'}
        disabled={pending || !objective.trim()}
        leading={pending ? <ActivityIndicator size="small" /> : undefined}
        onPress={() => void run({
          kind: 'set', objective: objective.trim(),
          status: goal && !replace && goal.status !== 'complete' && goal.status !== 'budgetLimited'
            ? goal.status : 'active',
          replace: replace && Boolean(goal),
        })}
      />
      {goal && !replace && (
        <>
          {(goal.status === 'active' || goal.status === 'paused' || goal.status === 'blocked' || goal.status === 'usageLimited') && <SheetRow
            label={goal.status === 'active' ? 'Pause goal' : 'Resume goal'}
            disabled={pending}
            onPress={() => void run({ kind: 'set', objective: null, status: goal.status === 'active' ? 'paused' : 'active', replace: false })}
          />}
          <SheetRow label="Clear goal" destructive disabled={pending} onPress={() => void run({ kind: 'clear' })} />
        </>
      )}
    </Sheet>
  );
}

function PlanUsageSheet({
  provider,
  accountId = null,
  providerVersion = null,
  onDismiss,
}: {
  provider: ProviderKind;
  accountId?: string | null;
  providerVersion?: string | null;
  onDismiss: () => void;
}) {
  const theme = useTheme();
  const daemon = useDaemon();
  const settings = useDaemonSettings();
  const profileId = daemon.activeProfile?.id ?? 'disconnected';
  const accountLabel = resolveProviderAccount(settings.data, provider, accountId)?.label ?? null;
  const usage = useQuery({
    queryKey: daemonKeys.planUsage(profileId, provider, accountId),
    queryFn: () => fetchPlanUsage(daemon.client!, provider, settings.data!, providerVersion, accountId),
    enabled: daemon.phase === 'connected' && Boolean(daemon.client && settings.data),
    staleTime: 30_000,
  });

  return (
    <Sheet
      onDismiss={onDismiss}
      title={`${providerLabel(provider)}${accountLabel ? ` · ${accountLabel}` : ''} usage`}
      visible
    >
      {usage.isPending || settings.isPending ? (
        <ActivityIndicator style={styles.loading} color={theme.textTertiary} />
      ) : usage.error ? (
        <>
          <Text style={[styles.note, { color: theme.danger }]}>
            {usage.error instanceof Error ? usage.error.message : String(usage.error)}
          </Text>
          <SheetRow label="Retry" onPress={() => void usage.refetch()} />
        </>
      ) : !usage.data ? (
        <Text style={[styles.note, { color: theme.textTertiary }]}>
          No plan usage reported for this account.
        </Text>
      ) : (
        <View style={styles.usageBody}>
          {usage.data.planLabel && (
            <Text style={[styles.planLabel, { color: theme.textSecondary }]}>
              {usage.data.planLabel}
            </Text>
          )}
          {usage.data.windows.length ? usage.data.windows.map((window) => {
            const percent = Math.min(100, Math.max(0, window.percent));
            const color = percent >= 95
              ? theme.danger
              : percent >= 80
                ? theme.warning
                : theme.textSecondary;
            const reset = window.resetsAt == null ? null : formatMobileResetLabel(window.resetsAt);
            return (
              <View key={`${window.label}-${window.resetsAt ?? 'none'}`} style={styles.usageRow}>
                <View style={styles.usageHeader}>
                  <Text numberOfLines={1} style={[styles.usageLabel, { color: theme.text }]}>
                    {window.label}
                  </Text>
                  {reset && (
                    <Text style={[styles.usageReset, { color: theme.textTertiary }]}>{reset}</Text>
                  )}
                  <Text style={[styles.usageValue, { color: theme.textSecondary }]}>
                    {`${window.percent.toFixed(0)}%`}
                  </Text>
                </View>
                <View style={[styles.usageTrack, { backgroundColor: theme.overlayStrong }]}>
                  <View style={[styles.usageFill, { backgroundColor: color, width: `${percent}%` }]} />
                </View>
              </View>
            );
          }) : (
            <Text style={[styles.note, { color: theme.textTertiary }]}>
              No active quota windows reported.
            </Text>
          )}
        </View>
      )}
    </Sheet>
  );
}

function formatMobileResetLabel(resetsAt: number): string {
  const seconds = resetsAt - Math.floor(Date.now() / 1_000);
  if (seconds <= 0) return 'Resets soon';
  const minutes = Math.ceil(seconds / 60);
  if (minutes < 60) return `Resets in ${minutes}m`;
  if (minutes < 24 * 60) {
    const hours = Math.floor(minutes / 60);
    const remainder = minutes % 60;
    return remainder ? `Resets in ${hours}h ${remainder}m` : `Resets in ${hours}h`;
  }
  return `Resets ${new Intl.DateTimeFormat(undefined, {
    weekday: 'short',
    hour: 'numeric',
    minute: '2-digit',
  }).format(new Date(resetsAt * 1_000))}`;
}

const styles = StyleSheet.create({
  input: { borderRadius: Radius.medium, fontSize: 15, minHeight: 44, paddingHorizontal: 12, paddingVertical: 10, marginHorizontal: 6, marginBottom: 8 },
  objective: { minHeight: 88, maxHeight: 180, textAlignVertical: 'top' },
  note: { fontSize: 13, lineHeight: 18, paddingHorizontal: 12, paddingVertical: 8 },
  loading: { paddingVertical: 20 },
  usageBody: { gap: 12, paddingHorizontal: 12, paddingVertical: 8 },
  planLabel: { fontSize: 12.5, fontWeight: '600' },
  usageRow: { gap: 6 },
  usageHeader: { alignItems: 'center', flexDirection: 'row', gap: 8 },
  usageLabel: { flex: 1, fontSize: 14, fontWeight: '500' },
  usageReset: { fontSize: 12 },
  usageValue: { fontSize: 12.5, fontVariant: ['tabular-nums'], fontWeight: '600' },
  usageTrack: { borderRadius: Radius.pill, height: 4, overflow: 'hidden' },
  usageFill: { borderRadius: Radius.pill, height: 4 },
});
