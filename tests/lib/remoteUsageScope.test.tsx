import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { UsageOriginContext } from "@/lib/query/usageOrigin";
import {
  useUsageSummaryByApp,
  useProviderStats,
  useModelStats,
  useUsageTrends,
  useRequestLogs,
  useUsageFirstDate,
} from "@/lib/query/usage";

const api = vi.hoisted(() => ({
  getUsageSummaryByApp: vi.fn().mockResolvedValue([]),
  getProviderStats: vi.fn().mockResolvedValue([]),
  getModelStats: vi.fn().mockResolvedValue([]),
  getUsageTrends: vi.fn().mockResolvedValue([]),
  getRequestLogs: vi.fn().mockResolvedValue({ data: [], total: 0 }),
  getUsageFirstDate: vi.fn().mockResolvedValue(null),
}));
vi.mock("@/lib/api/usage", () => ({ usageApi: api }));

describe("remote usage origin", () => {
  it("applies host scope to totals, charts, filters, and paginated requests", async () => {
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    const range = { preset: "30d" as const };
    const wrapper = ({ children }: { children: React.ReactNode }) => (
      <QueryClientProvider client={client}>
        <UsageOriginContext.Provider value="host:server-a">
          {children}
        </UsageOriginContext.Provider>
      </QueryClientProvider>
    );
    renderHook(
      () => {
        useUsageSummaryByApp(range);
        useProviderStats(range);
        useModelStats(range);
        useUsageTrends(range);
        useUsageFirstDate();
        useRequestLogs({ filters: {}, range });
      },
      { wrapper },
    );
    await waitFor(() => expect(api.getRequestLogs).toHaveBeenCalled());
    for (const fn of [
      api.getUsageSummaryByApp,
      api.getProviderStats,
      api.getModelStats,
      api.getUsageTrends,
      api.getUsageFirstDate,
    ]) {
      expect(fn.mock.calls.at(-1)?.at(-1)).toBe("host:server-a");
    }
    expect(api.getRequestLogs).toHaveBeenCalledWith(
      expect.objectContaining({ origin: "host:server-a" }),
      0,
      20,
    );
    client.clear();
  });
});
