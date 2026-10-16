import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { RemoteProviderPage } from "@/components/remote/RemoteProviderPage";
import { providersApi } from "@/lib/api/providers";

describe("remote Grok configuration preview", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    vi.spyOn(providersApi, "getSshHosts").mockResolvedValue([]);
    vi.spyOn(providersApi, "getRemoteCliVersions").mockResolvedValue({
      hostAlias: "test-host",
      clis: [],
    });
    vi.spyOn(providersApi, "getRemoteGatewayState").mockResolvedValue({
      hostKey: "test-host",
      app: "grokbuild",
      enabled: false,
      tunnel: { state: "idle" },
      proxyRunning: false,
    });
  });

  it.each([
    '[models]\ndefault = "custom"\n[model.custom]\nmodel = "grok-4.5"\napi_key = "test-secret"\n',
    '[model.custom]\napi_key = "test-secret"\n[broken',
  ])("hides TOML credentials, including on a parse failure", async (config) => {
    const inspect = vi.spyOn(providersApi, "inspectRemote").mockResolvedValue({
      hostAlias: "test-host",
      app: "grokbuild",
      provider: {
        id: "remote-current",
        name: "Remote current",
        settingsConfig: { config },
        category: "custom",
      },
      files: [
        { path: "~/.grok/config.toml", exists: true, bytes: config.length },
      ],
      hasExistingConfig: true,
      hasUnmanagedConfig: true,
      warnings: [],
    });
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    const { container } = render(
      <QueryClientProvider client={client}>
        <RemoteProviderPage
          appId="grokbuild"
          providers={{}}
          currentProviderId=""
          focusTarget={{
            target: { type: "config", alias: "test-host" },
            nonce: 1,
          }}
        />
      </QueryClientProvider>,
    );
    await waitFor(() =>
      expect(inspect).toHaveBeenCalledWith("grokbuild", {
        type: "config",
        alias: "test-host",
      }),
    );
    await waitFor(() =>
      expect(container.querySelector("pre")?.textContent).toContain("********"),
    );
    expect(container.textContent).not.toContain("test-secret");
    if (!config.includes("[broken")) {
      expect(container.querySelector("pre")?.textContent).toContain("grok-4.5");
    }
    client.clear();
  });

  it("keeps CLI versions and usage on their own tab", async () => {
    vi.spyOn(providersApi, "inspectRemote").mockResolvedValue({
      hostAlias: "test-host",
      app: "grokbuild",
      provider: null,
      files: [],
      hasExistingConfig: false,
      hasUnmanagedConfig: false,
      warnings: [],
    });
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    render(
      <QueryClientProvider client={client}>
        <RemoteProviderPage
          appId="grokbuild"
          providers={{}}
          currentProviderId=""
          focusTarget={{
            target: { type: "config", alias: "test-host" },
            nonce: 1,
          }}
        />
      </QueryClientProvider>,
    );
    const hostTab = await screen.findByRole("tab", { name: "CLI 与用量" });
    expect(screen.getByText("切换远端供应商")).toBeInTheDocument();
    expect(screen.queryByText("remoteUsage.title")).toBeNull();
    fireEvent.click(hostTab);
    expect(await screen.findByText("remoteUsage.title")).toBeInTheDocument();
    expect(screen.queryByText("切换远端供应商")).toBeNull();
    client.clear();
  });
});
