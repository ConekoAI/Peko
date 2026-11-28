import { useQuery, useQueryClient } from "@tanstack/react-query";

import SetupWizard from "./SetupWizard";
import { appSetupStatus } from "../../lib/api";

const KEY = ["app-setup"];

/**
 * Shows the first-run setup instead of the app while this computer has no
 * Peko folder (src-tauri/src/home.rs holds the engine until then). Once
 * setup is done every query starts over, now against the engine it started.
 */
export default function SetupGate({ children }: { children: React.ReactNode }) {
  const qc = useQueryClient();
  const { data: status, isPending } = useQuery({
    queryKey: KEY,
    queryFn: appSetupStatus,
    staleTime: Infinity,
    retry: false,
  });

  // Asked once at start; a blank window for that moment, not a flash of
  // the app that then turns into setup.
  if (isPending) return <div className="h-screen w-screen bg-surface-rail" />;
  if (status?.needed) {
    return (
      <SetupWizard
        status={status}
        onDone={() => {
          qc.setQueryData(KEY, { ...status, needed: false, reason: null });
          void qc.invalidateQueries({ predicate: (q) => q.queryKey[0] !== KEY[0] });
        }}
      />
    );
  }
  return <>{children}</>;
}
