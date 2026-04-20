import { writable, type Writable } from "svelte/store";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

export type UpdateState =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "up-to-date" }
  | { kind: "available"; version: string; notes?: string }
  | { kind: "downloading"; version: string; pct: number }
  | { kind: "ready"; version: string }
  | { kind: "error"; message: string };

/// Global store consumed by the sidebar banner. Non-blocking: the rest of the
/// app keeps working while an update is checked / downloaded / staged.
export const updateState: Writable<UpdateState> = writable({ kind: "idle" });

let currentUpdate: Update | null = null;

/// Kick off an update check. Background checks are silent — stay idle on
/// "no update" or network failure. Manual checks pass `manual = true` so the
/// UI can surface a transient "up to date" / error chip as feedback.
export async function checkForUpdate(manual = false): Promise<void> {
  updateState.set({ kind: "checking" });
  try {
    const u = await check();
    if (!u) {
      if (manual) {
        updateState.set({ kind: "up-to-date" });
        // Clear the transient chip after a few seconds so the sidebar
        // doesn't stay noisy forever.
        setTimeout(() => {
          updateState.update((s) => (s.kind === "up-to-date" ? { kind: "idle" } : s));
        }, 4000);
      } else {
        updateState.set({ kind: "idle" });
      }
      return;
    }
    currentUpdate = u;
    updateState.set({
      kind: "available",
      version: u.version,
      notes: u.body ?? undefined,
    });
  } catch (e) {
    console.warn("[updater] check failed:", e);
    if (manual) {
      updateState.set({ kind: "error", message: String(e) });
      setTimeout(() => {
        updateState.update((s) => (s.kind === "error" ? { kind: "idle" } : s));
      }, 5000);
    } else {
      updateState.set({ kind: "idle" });
    }
  }
}

/// Download + stage the update. On success the binary is patched, the caller
/// decides when to relaunch (we do it immediately).
export async function installUpdate(): Promise<void> {
  const u = currentUpdate;
  if (!u) return;
  const version = u.version;
  let downloaded = 0;
  let total = 0;
  updateState.set({ kind: "downloading", version, pct: 0 });
  try {
    await u.downloadAndInstall((event) => {
      if (event.event === "Started") {
        total = event.data.contentLength ?? 0;
      } else if (event.event === "Progress") {
        downloaded += event.data.chunkLength;
        const pct = total > 0 ? Math.min(100, (downloaded / total) * 100) : 0;
        updateState.set({ kind: "downloading", version, pct });
      } else if (event.event === "Finished") {
        updateState.set({ kind: "ready", version });
      }
    });
    // Triggers the platform-specific relaunch (NSIS passive installer on Win,
    // mount+replace on macOS, AppImage overwrite on Linux).
    await relaunch();
  } catch (e) {
    updateState.set({ kind: "error", message: String(e) });
  }
}

/// Kick an initial check on mount, then repeat every 6 h. The sidebar banner
/// reads the store so an update appearing mid-session is surfaced without a
/// restart.
export function startUpdateWatcher(): () => void {
  void checkForUpdate();
  const id = setInterval(() => void checkForUpdate(), 6 * 60 * 60 * 1000);
  return () => clearInterval(id);
}
