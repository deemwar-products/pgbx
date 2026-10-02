import { createContext, useContext, useEffect, useState } from 'react';
import { api, setProfile, type Session } from './api';
import { ConfirmDialog, ErrorCard, Loading, type ConfirmSpec } from './ui';
import { OverviewScreen } from './screens/Overview';
import { DbScreen } from './screens/Db';
import { RestoreScreen } from './screens/Restore';
import { QueryScreen } from './screens/Query';
import { HealthScreen } from './screens/Health';

export interface Ctx {
  session: Session;
  profile: string; // '' = the connection pgbx serve was started with
  /** `--profile P` to append to a CLI command, or '' */
  profileFlag: string;
  /** bumps when the connection changes: screens reload */
  gen: number;
  confirm: (s: ConfirmSpec) => void;
  toast: (m: string) => void;
}

const AppCtx = createContext<Ctx | null>(null);
export function useApp(): Ctx {
  const c = useContext(AppCtx);
  if (!c) throw new Error('no app context');
  return c;
}

export function go(path: string) {
  location.hash = '#/' + path;
}

function useRoute(): string[] {
  const read = () => (location.hash.replace(/^#\/?/, '') || 'overview').split('/').map((p) => decodeURIComponent(p));
  const [r, setR] = useState(read);
  useEffect(() => {
    const f = () => setR(read());
    window.addEventListener('hashchange', f);
    return () => window.removeEventListener('hashchange', f);
  }, []);
  return r;
}

const THEME_KEY = 'pgbx-serve-theme';
function initialTheme(): string {
  try {
    return localStorage.getItem(THEME_KEY) || '';
  } catch {
    return '';
  }
}

const TABS = [
  ['overview', 'Overview'],
  ['restore', 'Restore'],
  ['query', 'Query'],
  ['health', 'Health'],
] as const;

export function App({ hasToken }: { hasToken: boolean }) {
  const route = useRoute();
  const [session, setSession] = useState<Session | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [profile, setProf] = useState<string>(() => {
    try {
      return sessionStorage.getItem('pgbx-serve-profile') ?? '';
    } catch {
      return '';
    }
  });
  const [gen, setGen] = useState(0);
  const [theme, setTheme] = useState(initialTheme);
  const [confirmSpec, setConfirm] = useState<ConfirmSpec | null>(null);
  const [toastMsg, setToast] = useState<string | null>(null);

  useEffect(() => {
    if (theme) document.documentElement.dataset.theme = theme;
    else delete document.documentElement.dataset.theme;
    try {
      if (theme) localStorage.setItem(THEME_KEY, theme);
      else localStorage.removeItem(THEME_KEY);
    } catch {
      /* per-viewer convenience only */
    }
  }, [theme]);

  useEffect(() => {
    setProfile(profile);
    api<Session>('/api/session')
      .then((s) => {
        setSession(s);
        setErr(null);
      })
      .catch((e) => setErr((e as Error).message));
  }, [profile]);

  useEffect(() => {
    if (!toastMsg) return;
    const t = setTimeout(() => setToast(null), 6000);
    return () => clearTimeout(t);
  }, [toastMsg]);

  const switchProfile = (p: string) => {
    try {
      sessionStorage.setItem('pgbx-serve-profile', p);
    } catch {
      /* ignore */
    }
    setProf(p);
    setGen((g) => g + 1);
  };

  const toggleTheme = () => {
    const dark = theme ? theme === 'dark' : matchMedia('(prefers-color-scheme: dark)').matches;
    setTheme(dark ? 'light' : 'dark');
  };

  if (!hasToken && !session) {
    return (
      <main>
        <ErrorCard title="Open the link pgbx serve printed" error="This page needs the per-run token that is in the link pgbx serve prints when it starts (…/#token=…). Run pgbx serve again if you lost it." />
      </main>
    );
  }
  if (!session) {
    return <main>{err ? <ErrorCard error={err} title="pgbx serve did not answer" /> : <Loading />}</main>;
  }

  const active = profile || session.start;
  const ctx: Ctx = {
    session,
    profile,
    profileFlag: active ? ` --profile ${active}` : '',
    gen,
    confirm: setConfirm,
    toast: setToast,
  };
  const [view, arg] = route;
  const names = new Set(session.profiles.map((p) => p.name));
  const options = [...(session.start && !names.has(session.start) ? [session.start] : []), ...session.profiles.map((p) => p.name)];

  return (
    <AppCtx.Provider value={ctx}>
      <header className="top">
        <div className="brand">
          <span className="logo" aria-hidden="true" />
          <b>pgbx</b>
          <span className="mute small">v{session.version}</span>
        </div>
        <label className="conn">
          <span className="sr">connection</span>
          <select value={active} onChange={(e) => switchProfile(e.target.value === session.start ? '' : e.target.value)} title="connection (pgbx profile list)">
            {!session.start && <option value="">flags / environment</option>}
            {options.map((n) => (
              <option key={n} value={n}>
                {n}
                {session.default === n ? ' (default)' : ''}
              </option>
            ))}
          </select>
        </label>
        <span className={`badge ${session.allow_safe ? 'warn' : 'ok'}`} title={session.allow_safe ? 'pgbx serve --allow-safe' : 'start with --allow-safe to enable safe actions'}>
          {session.allow_safe ? 'safe actions on' : 'read-only'}
        </span>
        <button type="button" className="btn ghost theme" onClick={toggleTheme} aria-label="toggle light or dark theme">
          ◐
        </button>
        <nav aria-label="screens">
          {TABS.map(([v, label]) => (
            <a key={v} href={`#/${v}`} className={view === v || (v === 'overview' && view === 'db') ? 'on' : ''}>
              {label}
            </a>
          ))}
        </nav>
      </header>
      {session.warnings.length > 0 && (
        <div className="banner warn">
          {session.warnings.map((w) => (
            <p key={w}>{w}</p>
          ))}
        </div>
      )}
      <main key={`${active}:${gen}`}>
        {view === 'db' && arg ? (
          <DbScreen name={arg} />
        ) : view === 'restore' ? (
          <RestoreScreen db={arg} backupId={route[2]} />
        ) : view === 'query' ? (
          <QueryScreen db={arg} />
        ) : view === 'health' ? (
          <HealthScreen />
        ) : (
          <OverviewScreen />
        )}
      </main>
      <ConfirmDialog
        spec={confirmSpec}
        onClose={(m) => {
          setConfirm(null);
          if (m) {
            setToast(m);
            setGen((g) => g + 1);
          }
        }}
      />
      {toastMsg && (
        <div className="toast" role="status" onClick={() => setToast(null)}>
          {toastMsg}
        </div>
      )}
    </AppCtx.Provider>
  );
}
