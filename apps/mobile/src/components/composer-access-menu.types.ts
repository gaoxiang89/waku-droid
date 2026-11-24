import type { ProviderKind, RuntimeMode } from '@waku/client';

export interface ComposerAccessMenuProps {
  mode: RuntimeMode;
  provider?: ProviderKind;
  onApply: (mode: RuntimeMode) => void;
}
