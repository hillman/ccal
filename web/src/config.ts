// ccal-server is always same-origin: in dev, vite proxies /sync to it; in
// production it serves this app. So the sync URL is derived from the page
// origin and never configured. Only the bearer token needs supplying (kept in
// localStorage; entered once via the gate in App).

export const DOC_ID = "ccal";

export function syncUrl(docId: string = DOC_ID): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}/sync/${docId}`;
}

const TOKEN_KEY = "ccal.token";

export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY);
}
export function setToken(token: string): void {
  localStorage.setItem(TOKEN_KEY, token);
}
export function clearToken(): void {
  localStorage.removeItem(TOKEN_KEY);
}
