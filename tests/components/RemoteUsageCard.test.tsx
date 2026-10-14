import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { RemoteUsageCard } from "@/components/remote/RemoteUsageCard";

const api = vi.hoisted(() => ({
  hosts: vi.fn(),
  configure: vi.fn(),
  sync: vi.fn(),
}));
vi.mock("@/lib/api/remoteUsage", () => ({ remoteUsageApi: api }));
vi.mock("@/lib/toast", () => ({
  toast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));
const target = { type: "config" as const, alias: "server-a" };
const host = {
  hostKey: "server-a",
  config: { enabled: true, allHistory: false, roots: {} },
  lastSyncAt: null,
  lastError: null,
};

function mount() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  render(
    <QueryClientProvider client={client}>
      <RemoteUsageCard target={target} hostLabel="server-a" />
    </QueryClientProvider>,
  );
  return client;
}

describe("RemoteUsageCard", () => {
  beforeEach(() => {
    api.hosts.mockResolvedValue([host]);
    api.configure.mockResolvedValue(undefined);
    api.sync.mockResolvedValue({
      imported: 1,
      skipped: 0,
      filesScanned: 1,
      errors: [],
      deferredFiles: 0,
    });
  });

  it("saves custom roots and history before manually syncing", async () => {
    const user = userEvent.setup();
    const client = mount();
    await waitFor(() =>
      expect(
        screen.getByRole("checkbox", { name: "remoteUsage.auto" }),
      ).toBeChecked(),
    );
    await user.click(
      screen.getByRole("checkbox", { name: "remoteUsage.history" }),
    );
    await user.click(screen.getByRole("button", { name: "remoteUsage.paths" }));
    await user.type(
      screen.getByRole("textbox", { name: "codex remoteUsage.paths" }),
      "/work/codex/sessions\n/work/codex/archived_sessions",
    );
    await user.click(screen.getByRole("button", { name: "remoteUsage.sync" }));
    await waitFor(() => expect(api.sync).toHaveBeenCalledWith(target));
    expect(api.configure).toHaveBeenCalledWith(target, {
      enabled: true,
      allHistory: true,
      roots: {
        codex: ["/work/codex/sessions", "/work/codex/archived_sessions"],
      },
    });
    expect(api.configure.mock.invocationCallOrder[0]).toBeLessThan(
      api.sync.mock.invocationCallOrder[0],
    );
    client.clear();
  });

  it("status refresh preserves unsaved changes and shows sync failure", async () => {
    const user = userEvent.setup();
    const client = mount();
    await waitFor(() =>
      expect(
        screen.getByRole("checkbox", { name: "remoteUsage.auto" }),
      ).toBeChecked(),
    );
    await user.click(
      screen.getByRole("checkbox", { name: "remoteUsage.history" }),
    );
    client.setQueryData(
      ["remoteUsageHosts"],
      [{ ...host, lastError: "offline", lastSyncAt: 123 }],
    );
    expect(await screen.findByRole("alert")).toHaveTextContent("offline");
    expect(
      screen.getByRole("checkbox", { name: "remoteUsage.history" }),
    ).toBeChecked();
    client.clear();
  });
});
