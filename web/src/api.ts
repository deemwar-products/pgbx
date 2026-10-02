// The only network code in the app: same-origin calls to the pgbx serve API, each carrying the per-run token.
// No other origin is ever contacted (the server's CSP says connect-src 'self' as well).

const TOKEN_KEY = 'pgbx-serve-token';

/** Take the token from the URL fragment pgbx serve printed (#token=...), keep it for this tab, then hide it. */
export function initToken(): string {
  const m = location.hash.match(/(?:^#|&)token=([0-9a-f]+)/);
  if (m) {
    try {
      sessionStorage.setItem(TOKEN_KEY, m[1]);
    } catch {
      /* storage blocked: keep it in memory only */
    }
    memToken = m[1];
    history.replaceState(null, '', location.pathname + '#/overview');
  }
  return token();
}

let memToken = '';
function token(): string {
  try {
    return sessionStorage.getItem(TOKEN_KEY) || memToken;
  } catch {
    return memToken;
  }
}

let profile = '';
export function setProfile(p: string) {
  profile = p;
}

export class ApiError extends Error {
  status: number;
  constructor(message: string, status: number) {
    super(message);
    this.status = status;
  }
}

export async function api<T = Json>(path: string, body?: unknown): Promise<T> {
  const url = profile ? `${path}${path.includes('?') ? '&' : '?'}profile=${encodeURIComponent(profile)}` : path;
  const headers: Record<string, string> = { 'X-Pgbx-Token': token() };
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  let r: Response;
  try {
    r = await fetch(url, {
      method: body === undefined ? 'GET' : 'POST',
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      cache: 'no-store',
      credentials: 'omit',
    });
  } catch {
    throw new ApiError('pgbx serve is not answering (stopped?). Start it again and open the new link.', 0);
  }
  const j = (await r.json().catch(() => ({ ok: false, error: `HTTP ${r.status}` }))) as { ok?: boolean; error?: string };
  if (!r.ok || j.ok === false) throw new ApiError(j.error || `HTTP ${r.status}`, r.status);
  return j as T;
}

// ---- shapes (only what the screens read; the server sends more)

export type Json = any;

export interface Session {
  version: string;
  allow_safe: boolean;
  safety: string;
  start: string;
  default: string | null;
  profiles: { name: string; default: boolean; host?: string; port?: string; user?: string; ssh?: string }[];
  profiles_error: string | null;
  memory: boolean;
  warnings: string[];
  actions: string[];
}

export interface OverviewDb {
  database: string;
  state?: string;
  schedule?: string;
  last_backup_at?: string | null;
  last_backup_age?: string | null;
  last_backup_size?: string | null;
  next_backup_at?: string | null;
  backups_kept?: number | null;
  last_verify?: string | null;
  last_error?: string | null;
  seen_at?: string | null;
  size?: string;
}

export interface Overview {
  backups: 'on' | 'off';
  info?: string;
  next_steps?: string[];
  connection: { role: string; read_only: string; server_time: string; server_name?: string | null; server_version?: string };
  databases: OverviewDb[];
  windows?: { database: string; window_cron: string | null; window_score: number | null; current_score: number | null; window_confidence: string | null }[];
}

export interface QueueJob {
  database: string;
  job_id: number;
  kind: string;
  trigger: string;
  state: string;
  position: number | null;
  slot: number | null;
  requested_at: string | null;
  started_at: string | null;
  detail: string | null;
  progress: string | null;
  eta_start: string | null;
  eta_finish: string | null;
  est_bytes: number | null;
  done_bytes: number | null;
}

export interface Suggestion {
  start_at: string | null;
  cron: string | null;
  score: number | null;
  confidence: string | null;
  current_schedule: string | null;
  current_score: number | null;
  window_hours: number | null;
  est_duration: string | null;
  days_sampled: number | null;
  apply_sql: string | null;
}

export interface Backup {
  id: number;
  taken_at: string;
  age: string;
  trigger: string;
  size: string;
  bytes: number;
  s3_key: string;
}

export interface HistoryRow {
  id: number;
  kind: string;
  trigger: string;
  state: string;
  who: string | null;
  at: string | null;
  requested_at: string | null;
  started: string | null;
  finished: string | null;
  s3_key: string | null;
  bytes: number | null;
  error: string | null;
  params: Json;
}

export interface DbDetail {
  database: string;
  status: Record<string, Json>;
  suggestion: Suggestion | null;
  backups: Backup[];
  history: HistoryRow[];
}

export interface Check {
  name: string;
  ok: boolean;
  detail: string;
  fix: string;
  warning?: boolean;
  info?: string;
}

export interface QueryResult {
  columns: { name: string; type: string }[];
  rows: Record<string, Json>[];
  row_count: number;
  truncated: boolean;
  database: string;
  user: string;
  elapsed_ms: number;
}

export interface Memory {
  enabled: boolean;
  path?: string;
  exists?: boolean;
  questions: { name: string; note: string; sql: string }[];
}
