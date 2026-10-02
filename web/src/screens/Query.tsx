import { useEffect, useState } from 'react';
import { api, type Json, type Memory, type Overview, type QueryResult } from '../api';
import { go, useApp } from '../App';
import { ErrorCard, Loading, useApi } from '../ui';

function cell(v: Json): string {
  if (v == null) return '';
  return typeof v === 'object' ? JSON.stringify(v) : String(v);
}

function csv(r: QueryResult): string {
  const q = (s: string) => (/[",\n\r]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s);
  const names = r.columns.map((c) => c.name);
  return [names.map(q).join(','), ...r.rows.map((row) => names.map((n) => q(cell(row[n]))).join(','))].join('\r\n') + '\r\n';
}

function download(name: string, type: string, text: string) {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export function QueryScreen({ db: routeDb }: { db?: string }) {
  const { gen, session, toast } = useApp();
  const ov = useApi<Overview>('/api/overview', [gen]);
  const dbs = ov.data?.databases.map((d) => d.database) ?? [];
  const db = routeDb || dbs[0] || '';
  const mem = useApi<Memory>(db && session.memory ? `/api/memory/${encodeURIComponent(db)}` : null, [gen]);
  const [sql, setSql] = useState('');
  const [maxRows, setMaxRows] = useState(1000);
  const [res, setRes] = useState<QueryResult | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [saving, setSaving] = useState(false);
  const [name, setName] = useState('');
  const [note, setNote] = useState('');
  const [saveErr, setSaveErr] = useState<string | null>(null);

  useEffect(() => {
    setRes(null);
    setErr(null);
  }, [db]);

  const run = async () => {
    if (!sql.trim() || !db) return;
    setBusy(true);
    setErr(null);
    try {
      setRes(await api<QueryResult>('/api/query', { db, sql, max_rows: maxRows }));
    } catch (e) {
      setRes(null);
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const save = async () => {
    setSaveErr(null);
    try {
      const r = await api<{ message: string }>(`/api/memory/${encodeURIComponent(db)}`, { name, note, sql });
      toast(r.message);
      setSaving(false);
      setName('');
      setNote('');
      void mem.reload();
    } catch (e) {
      setSaveErr((e as Error).message);
    }
  };

  if (ov.error) return <ErrorCard error={ov.error} />;
  if (!ov.data) return <Loading />;

  return (
    <>
      <div className="card">
        <div className="row spread wrap-row">
          <h1 className="h1">Query</h1>
          <label className="inline">
            <span className="mute small">database </span>
            <select value={db} onChange={(e) => go(`query/${encodeURIComponent(e.target.value)}`)}>
              {dbs.map((d) => (
                <option key={d}>{d}</option>
              ))}
            </select>
          </label>
        </div>
        <p className="mute small">
          Read-only, through the same guard as <code>pgbx query</code>: one SELECT / WITH / TABLE / VALUES / SHOW / EXPLAIN statement, inside BEGIN READ ONLY,
          then ROLLBACK. A best-effort guard, not a security boundary: use a read-only role.
        </p>
        <textarea
          className="sql"
          value={sql}
          spellCheck={false}
          placeholder="SELECT * FROM pgbx.status()"
          onChange={(e) => setSql(e.target.value)}
          onKeyDown={(e) => {
            if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') {
              e.preventDefault();
              void run();
            }
          }}
          rows={6}
          aria-label="SQL"
        />
        <div className="row gap wrap-row">
          <button type="button" className="btn primary" onClick={run} disabled={busy || !sql.trim()}>
            {busy ? 'running…' : 'Run'}
          </button>
          <span className="mute small">Ctrl/⌘+Enter</span>
          <label className="inline small">
            max rows{' '}
            <input type="number" min={1} max={10000} value={maxRows} onChange={(e) => setMaxRows(Math.max(1, Math.min(10000, Number(e.target.value) || 1)))} />
          </label>
          {session.memory && (
            <button type="button" className="btn" disabled={!sql.trim()} onClick={() => setSaving((s) => !s)}>
              Save to memory…
            </button>
          )}
        </div>
        {saving && (
          <div className="savebox">
            <p className="small mute">
              Appends this question to {mem.data?.path ?? `~/pgbx/<connection>/${db}/memories.md`} (never rewrites what is there). Store SQL and notes only,
              never passwords or row data.
            </p>
            <label>
              <span>name</span>
              <input value={name} onChange={(e) => setName(e.target.value)} placeholder="orders today" />
            </label>
            <label>
              <span>one line of meaning</span>
              <input value={note} onChange={(e) => setNote(e.target.value)} placeholder="Orders placed in the last 24 h. created_at is UTC." />
            </label>
            {saveErr && <p className="bad small">{saveErr}</p>}
            <button type="button" className="btn primary" disabled={!name.trim()} onClick={save}>
              Save to memory
            </button>
          </div>
        )}
      </div>

      {err && <ErrorCard error={err} title="Refused or failed" />}
      {res && <Result r={res} />}

      {session.memory && (
        <div className="card">
          <h2>Saved questions</h2>
          {mem.error ? (
            <p className="bad">{mem.error}</p>
          ) : !mem.data ? (
            <p className="mute">loading…</p>
          ) : mem.data.questions.length === 0 ? (
            <p className="mute small">
              None yet for {db}. They are read from <code>{mem.data.path}</code>
              {mem.data.exists ? '' : ' (not created yet)'}.
            </p>
          ) : (
            <ul className="questions">
              {mem.data.questions.map((q) => (
                <li key={q.name}>
                  <button type="button" className="linkish" onClick={() => setSql(q.sql)} title={q.sql}>
                    {q.name}
                  </button>
                  {q.note && <span className="mute small"> {q.note}</span>}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </>
  );
}

function Result({ r }: { r: QueryResult }) {
  const base = `pgbx-${r.database}-${new Date().toISOString().slice(0, 19).replace(/[:T]/g, '')}`;
  return (
    <div className="card">
      <div className="row spread wrap-row">
        <h2>
          {r.row_count} row{r.row_count === 1 ? '' : 's'}
          {r.truncated && <span className="warn small"> (truncated at max rows)</span>}
        </h2>
        <div className="row gap">
          <span className="mute small">
            {r.elapsed_ms} ms · as {r.user}
          </span>
          <button type="button" className="btn small" onClick={() => download(`${base}.csv`, 'text/csv', csv(r))}>
            CSV
          </button>
          <button
            type="button"
            className="btn small"
            onClick={() => download(`${base}.json`, 'application/json', JSON.stringify({ columns: r.columns, rows: r.rows }, null, 2) + '\n')}
          >
            JSON
          </button>
        </div>
      </div>
      <div className="scroll result">
        <table className="data">
          <thead>
            <tr>
              {r.columns.map((c) => (
                <th key={c.name} title={c.type}>
                  {c.name}
                  <span className="mute type">{c.type}</span>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {r.rows.map((row, i) => (
              <tr key={i}>
                {r.columns.map((c) => {
                  const v = row[c.name];
                  return (
                    <td key={c.name} className={v == null ? 'null' : typeof v === 'number' ? 'num' : undefined}>
                      {v == null ? 'NULL' : cell(v)}
                    </td>
                  );
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
