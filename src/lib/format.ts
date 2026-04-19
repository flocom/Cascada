import type { Account } from "./api";

export const formatTime = (ts: number) => new Date(ts).toLocaleTimeString();

export const accountIndex = (accounts: Account[]): Map<string, Account> => {
  const m = new Map<string, Account>();
  for (const a of accounts) m.set(a.id, a);
  return m;
};

export const labelOf = (idx: Map<string, Account>, id: string) =>
  idx.get(id)?.label ?? "—";

export const platformOf = (idx: Map<string, Account>, id: string) =>
  idx.get(id)?.platform ?? "";
