import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { api } from './api';

// ---- formatting

export function ts(v: string | null | undefined): string {
  if (!v) return '—';
  const d = new Date(v);
  if (isNaN(d.getTime())) return String(v);
  return d.toISOString().replace('T', ' ').replace(/\.\d+Z$/, 'Z');
}

export function ago(v: string | null | undefined): string {
  if (!v) return '';
  const s = (Date.now() - Date.parse(v)) / 1000;
  if (isNaN(s)) return '';
  const a = Math.abs(s);
  const f = a < 90 ? `${Math.round(a)} s` : a < 3600 ? `${Math.round(a / 60)} min` : a < 172800 ? `${Math.round(a / 3600)} h` : `${Math.round(a / 86400)} d`;
  return s >= 0 ? `${f} ago` : `in ${f}`;
}

export type Tone = 'ok' | 'warn' | 'bad' | 'info' | 'mute';

/** Colour of a backup / job / check state. */
export function tone(state: string | null | undefined): Tone {
  const s = String(state || '').toLowerCase();
  if (/fail|error|overdue|missing|broken|unreachable/.test(s)) return 'bad';
  if (/cancel/.test(s)) return 'mute';
  if (/paus|wait|defer|queued|never|expired|first/.test(s)) return 'warn';
  if (/run|restor|verif.*progress/.test(s)) return 'info';
  if (/ok|done|active|pass|healthy|on\b/.test(s)) return 'ok';
  return 'mute';
}

export function Badge({ children, t }: { children: ReactNode; t?: Tone }) {
  const text = typeof children === 'string' ? children : '';
  return <span className={`badge ${t ?? tone(text)}`}>{children}</span>;
}

// ---- data loading

export function useApi<T>(path: string | null, deps: unknown[] = [], refreshMs = 0) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const seq = useRef(0);
  const load = useCallback(async () => {
    if (!path) return;
    const n = ++seq.current;
    setLoading(true);
    try {
      const j = await api<T>(path);
      if (n === seq.current) {
        setData(j);
        setError(null);
      }
    } catch (e) {
      if (n === seq.current) setError((e as Error).message);
    } finally {
      if (n === seq.current) setLoading(false);
    }
  }, [path, ...deps]);
  useEffect(() => {
    setData(null);
    setError(null);
    void load();
  }, [load]);
  useEffect(() => {
    if (!refreshMs) return;
    const t = setInterval(() => {
      if (document.visibilityState === 'visible') void load();
    }, refreshMs);
    return () => clearInterval(t);
  }, [load, refreshMs]);
  return { data, error, loading, reload: load };
}

export function Loading({ what }: { what?: string }) {
  return <div className="card mute">loading{what ? ` ${what}` : ''}…</div>;
}

export function ErrorCard({ error, title }: { error: string; title?: string }) {
  return (
    <div className="card error" role="alert">
      <h2>{title ?? 'Could not load this'}</h2>
      <p className="wrap">{error}</p>
    </div>
  );
}

// ---- copy to clipboard

export function Copy({ text, label = 'Copy' }: { text: string; label?: string }) {
  const [done, setDone] = useState<string | null>(null);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setDone('copied');
    } catch {
      setDone('select and copy');
    }
    setTimeout(() => setDone(null), 1800);
  };
  return (
    <button type="button" className="btn small" onClick={copy} title={text}>
      {done ?? label}
    </button>
  );
}

export function CommandLine({ cmd, label }: { cmd: string; label?: string }) {
  return (
    <div className="cmd">
      <code>{cmd}</code>
      <Copy text={cmd} label={label} />
    </div>
  );
}

// ---- confirm dialog (every action asks first)

export interface ConfirmSpec {
  title: string;
  body: ReactNode;
  cli: string;
  confirm: string;
  run: () => Promise<unknown>;
}

export function ConfirmDialog({ spec, onClose }: { spec: ConfirmSpec | null; onClose: (result?: string) => void }) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (spec && !d.open) {
      setErr(null);
      setBusy(false);
      d.showModal();
    }
    if (!spec && d.open) d.close();
  }, [spec]);
  if (!spec) return <dialog ref={ref} />;
  const go = async () => {
    setBusy(true);
    setErr(null);
    try {
      const r = (await spec.run()) as { message?: string; job_id?: number; state?: string };
      onClose(r?.message ?? (r?.job_id ? `queued as job ${r.job_id}` : 'done'));
    } catch (e) {
      setErr((e as Error).message);
      setBusy(false);
    }
  };
  return (
    <dialog ref={ref} onCancel={() => onClose()} aria-labelledby="dlg-title">
      <h2 id="dlg-title">{spec.title}</h2>
      <div className="dlg-body">{spec.body}</div>
      <p className="mute small">Same as running:</p>
      <CommandLine cmd={spec.cli} />
      {err && <p className="bad wrap">{err}</p>}
      <div className="row end">
        <button type="button" className="btn" onClick={() => onClose()} disabled={busy}>
          Cancel
        </button>
        <button type="button" className="btn primary" onClick={go} disabled={busy} autoFocus>
          {busy ? 'working…' : spec.confirm}
        </button>
      </div>
    </dialog>
  );
}

// ---- tables that become cards on a phone

export interface Col<T> {
  h: string;
  c: (r: T) => ReactNode;
  wide?: boolean;
}

export function Table<T>({ cols, rows, empty, rowClass }: { cols: Col<T>[]; rows: T[]; empty?: string; rowClass?: (r: T) => string }) {
  if (!rows.length) return <p className="mute">{empty ?? 'none'}</p>;
  return (
    <div className="scroll">
      <table className="grid">
        <thead>
          <tr>
            {cols.map((c) => (
              <th key={c.h}>{c.h}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((r, i) => (
            <tr key={i} className={rowClass?.(r)}>
              {cols.map((c) => (
                <td key={c.h} data-h={c.h} className={c.wide ? 'wrap' : undefined}>
                  {c.c(r)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** Quote a value for a shell command line only when it needs it. */
export function shq(v: string): string {
  return /^[A-Za-z0-9_.:@%+=,/-]+$/.test(v) ? v : `'${v.replace(/'/g, `'\\''`)}'`;
}
