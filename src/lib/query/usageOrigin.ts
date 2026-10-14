import { createContext, useContext } from "react";

/** Origin applies to every statistics query under a dashboard. */
export const UsageOriginContext = createContext<string | undefined>(undefined);
export const useUsageOrigin = () => useContext(UsageOriginContext);
