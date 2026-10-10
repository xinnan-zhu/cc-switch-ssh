import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { RemoteCliCard } from "@/components/remote/RemoteCliCard";
import {
  providersApi,
  type RemoteCliInfo,
  type SshConnectionTarget,
} from "@/lib/api/providers";

const target: SshConnectionTarget = { type: "config", alias: "dev-box" };

const cli = (overrides: Partial<RemoteCliInfo>): RemoteCliInfo => ({
  app: "claude",
  tool: "claude",
  path: "/home/u/.local/bin/claude",
  version: "2.1.0",
  latestVersion: "2.1.0",
  source: "native",
  updatable: true,
  error: null,
  ...overrides,
});

const report = () => ({
  hostAlias: "dev-box",
  clis: [
    cli({
      app: "claude",
      tool: "claude",
      version: "2.1.0",
      latestVersion: "2.2.0",
    }),
    cli({
      app: "codex",
      tool: "codex",
      path: "/home/u/.nvm/versions/node/v22/bin/codex",
      version: "0.150.0",
      latestVersion: "0.160.1",
      source: "npm",
    }),
    cli({
      app: "gemini",
      tool: "gemini",
      path: "/usr/bin/gemini",
      version: "0.9.0",
      latestVersion: "0.9.0",
      source: "npm",
    }),
    cli({
      app: "grokbuild",
      tool: "grok",
      path: null,
      version: null,
      latestVersion: null,
      source: "missing",
      updatable: false,
    }),
  ],
});

const renderCard = (onRestartProcesses = vi.fn()) => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  render(
    <QueryClientProvider client={client}>
      <RemoteCliCard
        target={target}
        targetKey="config:dev-box"
        hostLabel="dev-box"
        appId="codex"
        onRestartProcesses={onRestartProcesses}
      />
    </QueryClientProvider>,
  );
  return client;
};

describe("RemoteCliCard", () => {
  let client: QueryClient | undefined;

  beforeEach(() => {
    vi.restoreAllMocks();
    vi.spyOn(providersApi, "getRemoteCliVersions").mockResolvedValue(report());
  });

  afterEach(() => client?.clear());

  it("lists the current app first and only offers updates for outdated CLIs", async () => {
    client = renderCard();
    await screen.findByText("0.150.0");

    // Tests run without locale resources, so names fall back to the tool.
    const rows = screen
      .getAllByText(/^(claude|codex|gemini|grok)$/)
      .map((node) => node.textContent);
    expect(rows).toEqual(["codex· 当前", "claude", "gemini", "grok"]);
    expect(screen.getByText("→ 0.160.1")).toBeInTheDocument();
    expect(screen.getByText("→ 2.2.0")).toBeInTheDocument();
    // Claude + Codex each get a button, plus "update all".
    expect(screen.getAllByRole("button", { name: /^更新$/ })).toHaveLength(2);
    expect(
      screen.getByRole("button", { name: "全部更新" }),
    ).toBeInTheDocument();
    expect(screen.getByText("未安装")).toBeInTheDocument();
  });

  it("updates every outdated CLI one after another", async () => {
    const order: string[] = [];
    let release: (() => void) | undefined;
    const update = vi
      .spyOn(providersApi, "updateRemoteCli")
      .mockImplementation(async (app) => {
        order.push(`start:${app}`);
        if (app === "codex") {
          await new Promise<void>((resolve) => {
            release = resolve;
          });
        }
        order.push(`end:${app}`);
        const before = report().clis.find((item) => item.app === app)!;
        return {
          hostAlias: "dev-box",
          previousVersion: before.version,
          cli: { ...before, version: before.latestVersion },
          command: "update",
        };
      });

    client = renderCard();
    fireEvent.click(await screen.findByRole("button", { name: "全部更新" }));

    await waitFor(() => expect(update).toHaveBeenCalledTimes(1));
    expect(update).toHaveBeenCalledWith("codex", target);
    // The second update waits for the first one.
    expect(order).toEqual(["start:codex"]);
    expect(screen.getByRole("button", { name: /等待中/ })).toBeDisabled();

    release?.();
    await waitFor(() => expect(update).toHaveBeenCalledTimes(2));
    expect(update).toHaveBeenLastCalledWith("claude", target);
    await waitFor(() =>
      expect(screen.queryByRole("button", { name: /^更新$/ })).toBeNull(),
    );
    expect(order).toEqual([
      "start:codex",
      "end:codex",
      "start:claude",
      "end:claude",
    ]);
  });

  it("keeps going after a failed update and refreshes the list", async () => {
    const versions = vi.mocked(providersApi.getRemoteCliVersions);
    const update = vi
      .spyOn(providersApi, "updateRemoteCli")
      .mockRejectedValueOnce(new Error("npm error code EACCES"))
      .mockResolvedValueOnce({
        hostAlias: "dev-box",
        previousVersion: "2.1.0",
        cli: cli({ version: "2.2.0", latestVersion: "2.2.0" }),
        command: "update",
      });

    client = renderCard();
    fireEvent.click(await screen.findByRole("button", { name: "全部更新" }));

    await waitFor(() => expect(update).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(versions).toHaveBeenCalledTimes(2));
  });
});
