// The first-run setup (src-tauri/src/home.rs): with no Peko folder the app
// shows the wizard instead of itself; it chooses a folder, saves the
// privacy choices there, and only then starts the engine.

import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import type { AppSetupStatus, FolderCheck } from "../lib/api";

vi.mock("../components/TitleBar", () => ({ default: () => null }));

const api = vi.hoisted(() => ({
  appSetupStatus: vi.fn(),
  appSetupCheckFolder: vi.fn(),
  appSetupUseFolder: vi.fn(),
  appSetupFinish: vi.fn(),
  pickFolder: vi.fn(),
  privacySet: vi.fn(),
  engineStatus: vi.fn(),
}));
vi.mock("../lib/api", () => api);

import SetupGate from "../components/setup/SetupGate";

const DEFAULT = "/Users/ada/.peko";

function status(over: Partial<AppSetupStatus> = {}): AppSetupStatus {
  return { needed: true, reason: "fresh", home: DEFAULT, defaultHome: DEFAULT, platform: "macos", ...over };
}

function folder(home: string, over: Partial<FolderCheck> = {}): FolderCheck {
  return { home, kind: "new", pekos: null, freeBytes: 5e11, warnings: [], problem: null, ...over };
}

function renderGate() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <SetupGate>
        <p>the app</p>
      </SetupGate>
    </QueryClientProvider>,
  );
}

describe("SetupGate", () => {
  beforeEach(() => {
    Object.values(api).forEach((f) => f.mockReset());
    api.appSetupCheckFolder.mockImplementation(async (p: string) =>
      p === DEFAULT ? folder(p, { kind: "default" }) : folder(`${p}/Peko`),
    );
    api.appSetupUseFolder.mockImplementation(async (p: string | null) => p ?? DEFAULT);
    api.privacySet.mockImplementation(async (c) => c);
    api.appSetupFinish.mockResolvedValue(undefined);
    api.engineStatus.mockResolvedValue({ kind: "running", pid: 1, version: "0.8.1", uptime_secs: 1 });
  });

  it("shows the app when there is a Peko folder", async () => {
    api.appSetupStatus.mockResolvedValue(status({ needed: false, reason: null }));
    renderGate();
    expect(await screen.findByText("the app")).toBeTruthy();
    expect(screen.queryByTestId("setup-wizard")).toBeNull();
  });

  it("chooses a folder, saves privacy there, then starts the engine", async () => {
    api.appSetupStatus.mockResolvedValue(status());
    api.pickFolder.mockResolvedValue("/Volumes/Data");
    renderGate();

    fireEvent.click(await screen.findByTestId("setup-start"));
    fireEvent.click(await screen.findByTestId("setup-folder-choose"));
    // A folder with things in it gets a `Peko` folder inside.
    expect(await screen.findByText(/keep its own folder inside it/)).toBeTruthy();
    await waitFor(() =>
      expect((screen.getByTestId("setup-folder-continue") as HTMLButtonElement).disabled).toBe(false),
    );
    fireEvent.click(screen.getByTestId("setup-folder-continue"));
    await waitFor(() => expect(api.appSetupUseFolder).toHaveBeenCalledWith("/Volumes/Data/Peko"));

    fireEvent.click(await screen.findByTestId("privacy-usage-toggle"));
    fireEvent.click(screen.getByTestId("setup-privacy-continue"));
    // Nothing started before the last step.
    expect(api.appSetupFinish).not.toHaveBeenCalled();
    fireEvent.click(await screen.findByTestId("setup-finish"));

    expect(await screen.findByText("the app")).toBeTruthy();
    expect(api.privacySet).toHaveBeenCalledWith({ crashReports: true, usageEvents: false, acknowledged: true });
    expect(api.privacySet.mock.invocationCallOrder[0]).toBeLessThan(
      api.appSetupFinish.mock.invocationCallOrder[0],
    );
  });

  it("keeps the default folder as `~/.peko` (no pointer)", async () => {
    api.appSetupStatus.mockResolvedValue(status());
    renderGate();
    fireEvent.click(await screen.findByTestId("setup-start"));
    await waitFor(() =>
      expect((screen.getByTestId("setup-folder-continue") as HTMLButtonElement).disabled).toBe(false),
    );
    fireEvent.click(screen.getByTestId("setup-folder-continue"));
    await waitFor(() => expect(api.appSetupUseFolder).toHaveBeenCalledWith(null));
  });

  it("refuses a folder it cannot use", async () => {
    api.appSetupStatus.mockResolvedValue(status());
    api.pickFolder.mockResolvedValue("/usr/local");
    api.appSetupCheckFolder.mockImplementation(async (p: string) =>
      p === DEFAULT ? folder(p, { kind: "default" }) : folder(p, { problem: "system" }),
    );
    renderGate();
    fireEvent.click(await screen.findByTestId("setup-start"));
    fireEvent.click(await screen.findByTestId("setup-folder-choose"));
    expect(await screen.findByText(/belongs to the system/)).toBeTruthy();
    expect((screen.getByTestId("setup-folder-continue") as HTMLButtonElement).disabled).toBe(true);
  });

  it("does not make a missing folder afresh: it waits for it to come back", async () => {
    const gone = "/Volumes/T7/Peko";
    api.appSetupStatus.mockResolvedValue(status({ reason: "missing", home: gone }));
    let there = false;
    api.appSetupCheckFolder.mockImplementation(async (p: string) =>
      p === DEFAULT
        ? folder(p, { kind: "default" })
        : there
          ? folder(p, { kind: "peko", pekos: 2 })
          : folder(p, { problem: "notWritable" }),
    );
    renderGate();

    // No welcome: this computer has had Peko before.
    expect(await screen.findByTestId("setup-still-missing")).toBeTruthy();
    expect((screen.getByTestId("setup-folder-continue") as HTMLButtonElement).disabled).toBe(true);

    there = true;
    fireEvent.click(screen.getByTestId("setup-retry"));
    expect(await screen.findByText(/with 2 pekos/)).toBeTruthy();
    expect((screen.getByTestId("setup-folder-continue") as HTMLButtonElement).disabled).toBe(false);
  });
});
