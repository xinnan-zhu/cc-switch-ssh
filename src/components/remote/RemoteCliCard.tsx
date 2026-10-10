import { useMemo, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertTriangle,
  ArrowUpCircle,
  CheckCircle2,
  Loader2,
  PackageCheck,
  RefreshCw,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "@/lib/toast";
import type { AppId } from "@/lib/api";
import {
  providersApi,
  type RemoteCliInfo,
  type RemoteCliReport,
  type SshConnectionTarget,
} from "@/lib/api/providers";
import { isUpdateAvailable } from "@/lib/version";
import { extractErrorMessage } from "@/utils/errorUtils";
import { cn } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";

interface RemoteCliCardProps {
  target: SshConnectionTarget;
  /** Stable key of `target`, used to cache the report per host. */
  targetKey: string;
  hostLabel: string;
  /** The app of the current page; listed first. */
  appId: AppId;
  /** Offered after the current app's CLI was updated. */
  onRestartProcesses?: () => void;
}

export const remoteCliQueryKey = (targetKey: string) => [
  "remoteCliVersions",
  targetKey,
];

const hasUpdate = (cli: RemoteCliInfo) =>
  cli.updatable && isUpdateAvailable(cli.version, cli.latestVersion);

export function RemoteCliCard({
  target,
  targetKey,
  hostLabel,
  appId,
  onRestartProcesses,
}: RemoteCliCardProps) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const queryKey = remoteCliQueryKey(targetKey);
  // Apps being updated; one at a time per host, since parallel npm installs
  // on the same prefix can clash.
  const [updating, setUpdating] = useState<AppId | null>(null);
  const [queued, setQueued] = useState<AppId[]>([]);
  const busyRef = useRef(false);

  const query = useQuery<RemoteCliReport>({
    queryKey,
    queryFn: () => providersApi.getRemoteCliVersions(target),
    staleTime: 5 * 60 * 1000,
  });

  const clis = useMemo(() => {
    const list = query.data?.clis ?? [];
    return [...list].sort(
      (a, b) => Number(b.app === appId) - Number(a.app === appId),
    );
  }, [appId, query.data?.clis]);
  const outdated = clis.filter(hasUpdate);

  const appName = (cli: RemoteCliInfo) =>
    t(`apps.${cli.app}`, { defaultValue: cli.tool });

  const runUpdates = async (apps: AppId[]) => {
    if (busyRef.current || apps.length === 0) return;
    busyRef.current = true;
    setQueued(apps);
    try {
      for (const app of apps) {
        setUpdating(app);
        const name = t(`apps.${app}`, { defaultValue: app });
        try {
          const result = await providersApi.updateRemoteCli(app, target);
          queryClient.setQueryData<RemoteCliReport>(queryKey, (report) =>
            report
              ? {
                  ...report,
                  clis: report.clis.map((cli) =>
                    cli.app === result.cli.app ? result.cli : cli,
                  ),
                }
              : report,
          );
          const changed = result.previousVersion !== result.cli.version;
          const restart =
            app === appId && onRestartProcesses
              ? {
                  label: t("remote.restartProcesses", {
                    defaultValue: "停止进程",
                  }),
                  onClick: onRestartProcesses,
                }
              : undefined;
          if (changed) {
            toast.success(
              t("remote.cli.updated", {
                defaultValue: "{{app}} 已更新：{{from}} → {{to}}",
                app: name,
                from: result.previousVersion ?? "?",
                to: result.cli.version ?? "?",
              }),
              {
                description: t("remote.cli.updatedHint", {
                  defaultValue: "已运行的 {{app}} 进程重启后才会使用新版本。",
                  app: name,
                }),
                action: restart,
              },
            );
          } else {
            toast.info(
              t("remote.cli.unchanged", {
                defaultValue:
                  "{{app}} 更新命令已完成，版本仍为 {{version}}。可能已是该安装渠道的最新版。",
                app: name,
                version: result.cli.version ?? "?",
              }),
            );
          }
        } catch (error) {
          toast.error(
            t("remote.cli.updateFailed", {
              defaultValue: "更新 {{app}} 失败",
              app: name,
            }),
            {
              description: extractErrorMessage(error),
              duration: 12000,
            },
          );
          // Keep going with the rest; refresh so the list reflects reality.
          void queryClient.invalidateQueries({ queryKey });
        }
        setQueued((rest) => rest.filter((item) => item !== app));
      }
    } finally {
      setUpdating(null);
      setQueued([]);
      busyRef.current = false;
    }
  };

  const busy = updating !== null;

  const renderStatus = (cli: RemoteCliInfo) => {
    if (cli.source === "missing") {
      return (
        <span className="text-muted-foreground">
          {t("remote.cli.notInstalled", { defaultValue: "未安装" })}
        </span>
      );
    }
    if (!cli.version) {
      return (
        <span
          className="flex min-w-0 items-center gap-1 text-amber-700 dark:text-amber-300"
          title={cli.error ?? undefined}
        >
          <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
          <span className="truncate">
            {t("remote.cli.versionUnknown", {
              defaultValue: "无法读取版本",
            })}
          </span>
        </span>
      );
    }
    return (
      <span className="flex min-w-0 flex-wrap items-center gap-x-1.5 font-mono">
        <span>{cli.version}</span>
        {hasUpdate(cli) ? (
          <span className="text-sky-700 dark:text-sky-300">
            → {cli.latestVersion}
          </span>
        ) : cli.latestVersion ? (
          <CheckCircle2 className="h-3.5 w-3.5 text-emerald-600" />
        ) : null}
      </span>
    );
  };

  const sourceLabel = (cli: RemoteCliInfo) => {
    switch (cli.source) {
      case "npm":
        return "npm";
      case "native":
        return t("remote.cli.sourceNative", { defaultValue: "官方安装" });
      case "brew":
        return "Homebrew";
      case "unknown":
        return t("remote.cli.sourceUnknown", { defaultValue: "未知来源" });
      default:
        return null;
    }
  };

  const renderAction = (cli: RemoteCliInfo) => {
    if (cli.source === "missing") return null;
    const isUpdating = updating === cli.app;
    const isQueued = !isUpdating && queued.includes(cli.app);
    if (!cli.updatable) {
      return (
        <span
          className="text-xs text-muted-foreground"
          title={t("remote.cli.manualHint", {
            defaultValue:
              "无法识别安装方式（{{path}}），请在服务器上手动更新。",
            path: cli.path ?? cli.tool,
          })}
        >
          {t("remote.cli.manualOnly", { defaultValue: "需手动更新" })}
        </span>
      );
    }
    const outdatedCli = hasUpdate(cli);
    // Up to date: the check mark next to the version says it all.
    if (!outdatedCli && cli.latestVersion && !isUpdating && !isQueued) {
      return null;
    }
    return (
      <Button
        size="sm"
        variant={outdatedCli ? "default" : "outline"}
        disabled={busy}
        onClick={() => void runUpdates([cli.app])}
      >
        {isUpdating ? (
          <Loader2 className="h-4 w-4 animate-spin" />
        ) : (
          <ArrowUpCircle className="h-4 w-4" />
        )}
        {isUpdating
          ? t("remote.cli.updating", { defaultValue: "更新中" })
          : isQueued
            ? t("remote.cli.queued", { defaultValue: "等待中" })
            : t("remote.cli.update", { defaultValue: "更新" })}
      </Button>
    );
  };

  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <PackageCheck className="h-4 w-4 text-muted-foreground" />
            <h2 className="text-base font-semibold">
              {t("remote.cli.title", { defaultValue: "远端 CLI 版本" })}
            </h2>
            {outdated.length > 0 && (
              <Badge
                variant="secondary"
                className="rounded-md bg-sky-100 text-sky-700 dark:bg-sky-900/40 dark:text-sky-300"
              >
                {t("remote.cli.updatesAvailable", {
                  defaultValue: "{{count}} 个可更新",
                  count: outdated.length,
                })}
              </Badge>
            )}
          </div>
          <p className="mt-1 text-xs text-muted-foreground">
            {t("remote.cli.description", {
              defaultValue:
                "{{host}} 上已安装的 Claude Code、Codex、Gemini CLI 和 Grok Build，按原安装方式（npm、官方安装或 Homebrew）更新。",
              host: hostLabel,
            })}
          </p>
        </div>
        <div className="flex items-center gap-2">
          {outdated.length > 1 && (
            <Button
              size="sm"
              disabled={busy}
              onClick={() => void runUpdates(outdated.map((cli) => cli.app))}
            >
              <ArrowUpCircle className="h-4 w-4" />
              {t("remote.cli.updateAll", { defaultValue: "全部更新" })}
            </Button>
          )}
          <Button
            variant="outline"
            size="icon"
            className="h-8 w-8"
            onClick={() => void query.refetch()}
            disabled={busy || query.isFetching}
            title={t("common.refresh")}
            aria-label={t("common.refresh")}
          >
            <RefreshCw
              className={cn("h-4 w-4", query.isFetching && "animate-spin")}
            />
          </Button>
        </div>
      </div>

      {query.isError ? (
        <div className="mt-4 rounded-md border border-destructive/30 bg-destructive/10 px-3 py-2 text-xs text-destructive">
          {extractErrorMessage(query.error)}
        </div>
      ) : query.isLoading ? (
        <div className="mt-4 flex items-center gap-2 text-xs text-muted-foreground">
          <Loader2 className="h-3.5 w-3.5 animate-spin" />
          {t("remote.cli.checking", { defaultValue: "正在检测远端版本..." })}
        </div>
      ) : (
        <div className="mt-4 divide-y divide-border rounded-md border border-border">
          {clis.map((cli) => (
            <div
              key={cli.app}
              className={cn(
                "flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2 text-sm",
                cli.source === "missing" && "opacity-60",
              )}
            >
              <div className="w-28 shrink-0 font-medium">
                {appName(cli)}
                {cli.app === appId && (
                  <span className="ml-1 text-xs font-normal text-muted-foreground">
                    · {t("remote.cli.currentApp", { defaultValue: "当前" })}
                  </span>
                )}
              </div>
              <div className="flex min-w-0 flex-1 items-center gap-2 text-xs">
                {renderStatus(cli)}
                {sourceLabel(cli) && (
                  <Badge
                    variant="secondary"
                    className="rounded-md px-1.5 py-0 text-[11px] font-normal"
                    title={cli.path ?? undefined}
                  >
                    {sourceLabel(cli)}
                  </Badge>
                )}
              </div>
              {renderAction(cli)}
            </div>
          ))}
        </div>
      )}
    </section>
  );
}
