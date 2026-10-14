import { invoke } from "@tauri-apps/api/core";
import type { SshConnectionTarget } from "./providers";
import type { SessionSyncResult } from "@/types/usage";

export interface RemoteUsageConfig {
  enabled: boolean;
  allHistory: boolean;
  roots: Record<string, string[]>;
}
export interface RemoteUsageHost {
  hostKey: string;
  config: RemoteUsageConfig;
  lastSyncAt: number | null;
  lastError: string | null;
}
export const remoteUsageApi = {
  hosts: () => invoke<RemoteUsageHost[]>("get_remote_usage_hosts"),
  configure: (target: SshConnectionTarget, config: RemoteUsageConfig) =>
    invoke<void>("configure_remote_usage", { target, config }),
  sync: (target: SshConnectionTarget) =>
    invoke<SessionSyncResult>("sync_remote_usage", { target }),
};
