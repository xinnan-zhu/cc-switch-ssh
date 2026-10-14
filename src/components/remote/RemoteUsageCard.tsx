import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Loader2, RefreshCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { remoteUsageApi, type RemoteUsageConfig } from "@/lib/api/remoteUsage";
import type { SshConnectionTarget } from "@/lib/api/providers";
import { usageKeys } from "@/lib/query/usage";
import { extractErrorMessage } from "@/utils/errorUtils";
import { toast } from "@/lib/toast";

const APPS = ["claude", "codex", "gemini", "grokbuild"] as const;

export function RemoteUsageCard({
  target,
  hostLabel,
}: {
  target: SshConnectionTarget;
  hostLabel: string;
}) {
  const { t } = useTranslation();
  const client = useQueryClient();
  const { data: hosts = [] } = useQuery({
    queryKey: ["remoteUsageHosts"],
    queryFn: remoteUsageApi.hosts,
    refetchInterval: 30000,
  });
  const saved = hosts.find((h) => h.hostKey === hostLabel);
  const [enabled, setEnabled] = useState(false);
  const [allHistory, setAllHistory] = useState(false);
  const [paths, setPaths] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const savedConfig = JSON.stringify(saved?.config ?? null);
  useEffect(() => {
    const config = JSON.parse(savedConfig) as RemoteUsageConfig | null;
    setEnabled(config?.enabled ?? false);
    setAllHistory(config?.allHistory ?? false);
    setPaths(
      Object.fromEntries(
        Object.entries(config?.roots ?? {}).map(([app, roots]) => [
          app,
          roots.join("\n"),
        ]),
      ),
    );
  }, [savedConfig]);

  const run = async (sync: boolean) => {
    setBusy(true);
    try {
      const config: RemoteUsageConfig = {
        enabled,
        allHistory,
        roots: Object.fromEntries(
          APPS.map((app) => [
            app,
            (paths[app] ?? "")
              .split("\n")
              .map((p) => p.trim())
              .filter(Boolean),
          ]).filter(([, roots]) => roots.length > 0),
        ),
      };
      await remoteUsageApi.configure(target, config);
      if (sync) {
        const result = await remoteUsageApi.sync(target);
        toast.success(
          t("remoteUsage.done", {
            count: result.imported,
            skipped: result.skipped,
          }),
        );
        if (result.errors.length) toast.error(result.errors.join("; "));
        if (result.deferredFiles) toast.info(t("remoteUsage.more"));
        await client.invalidateQueries({ queryKey: usageKeys.all });
      } else {
        toast.success(t("remoteUsage.saved"));
      }
    } catch (error) {
      toast.error(extractErrorMessage(error));
    } finally {
      await client.invalidateQueries({ queryKey: ["remoteUsageHosts"] });
      setBusy(false);
    }
  };

  return (
    <section className="rounded-panel border border-border bg-surface p-4 space-y-3">
      <h3 className="text-section text-fg-1">{t("remoteUsage.title")}</h3>
      <p className="text-caption text-fg-2">{t("remoteUsage.description")}</p>
      <div className="flex flex-wrap items-center gap-4 text-body">
        <label className="flex items-center gap-2">
          <input
            type="checkbox"
            checked={enabled}
            disabled={
              busy || Boolean(target.type === "manual" && target.password)
            }
            onChange={(e) => setEnabled(e.target.checked)}
          />
          {t("remoteUsage.auto")}
        </label>
        <label className="flex items-center gap-2">
          <input
            type="checkbox"
            checked={allHistory}
            disabled={busy}
            onChange={(e) => setAllHistory(e.target.checked)}
          />
          {t("remoteUsage.history")}
        </label>
        <Button
          variant="quiet"
          size="compact"
          onClick={() => setExpanded(!expanded)}
        >
          {t("remoteUsage.paths")}
        </Button>
      </div>
      {expanded && (
        <div className="grid gap-3 sm:grid-cols-2">
          {APPS.map((app) => (
            <label key={app} className="text-caption text-fg-2">
              {t(`apps.${app}`, { defaultValue: app })}
              <textarea
                aria-label={`${app} ${t("remoteUsage.paths")}`}
                value={paths[app] ?? ""}
                disabled={busy}
                onChange={(e) => setPaths({ ...paths, [app]: e.target.value })}
                placeholder={t("remoteUsage.defaultPaths")}
                className="mt-1 block w-full rounded-control border border-border bg-surface p-2"
                rows={2}
              />
            </label>
          ))}
        </div>
      )}
      <p className="text-caption text-fg-3">{t("remoteUsage.gatewayPolicy")}</p>
      {saved?.lastSyncAt && (
        <p className="text-caption text-fg-2">
          {t("remoteUsage.lastSync")}:{" "}
          {new Date(saved.lastSyncAt * 1000).toLocaleString()}
        </p>
      )}
      {saved?.lastError && (
        <p role="alert" className="text-caption text-red-500">
          {saved.lastError}
        </p>
      )}
      <div className="flex gap-2">
        <Button
          variant="neutral"
          size="compact"
          disabled={busy}
          onClick={() => void run(false)}
        >
          {t("remoteUsage.save")}
        </Button>
        <Button
          variant="neutral"
          size="compact"
          disabled={busy}
          onClick={() => void run(true)}
        >
          {busy ? (
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
          ) : (
            <RefreshCw className="h-3.5 w-3.5" />
          )}
          {t("remoteUsage.sync")}
        </Button>
      </div>
    </section>
  );
}
