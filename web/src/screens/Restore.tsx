import { useEffect, useMemo, useState } from 'react';
import { api, type Backup, type DbDetail, type Overview } from '../api';
import { go, useApp } from '../App';
import { ago, CommandLine, ErrorCard, Loading, shq, ts, useApi } from '../ui';

/** A backup's taken_at as a --time that selects exactly it: UTC, rounded UP to the second (restore picks the newest
 *  backup at or before --time, so rounding down could pick the one before). */
export function backupTime(taken: string): string {
  const ms = Date.parse(taken);
  const frac = /\.(\d+)/.exec(taken)?.[1] ?? '';
  const up = /[1-9]/.test(frac) ? Math.floor(ms / 1000) * 1000 + 1000 : ms;
  return new Date(up).toISOString().replace('T', ' ').replace(/\.\d+Z$/, '+00');
}

/** datetime-local value (UTC) -> '2026-01-31 14:00:00+00' */
function fromInput(v: string): string {
  if (!v) return '';
  const s = v.length === 16 ? `${v}:00` : v;
  return `${s.replace('T', ' ')}+00`;
}

function stamp(): string {
  const d = new Date().toISOString();
  return d.slice(0, 10).replace(/-/g, '') + '_' + d.slice(11, 16).replace(':', '');
}

export function RestoreScreen({ db: initialDb, backupId }: { db?: string; backupId?: string }) {
  const { gen, session, confirm, profileFlag } = useApp();
  const ov = useApi<Overview>('/api/overview', [gen]);
  const dbs = ov.data?.databases.map((d) => d.database) ?? [];
  const [db, setDb] = useState(initialDb ?? '');
  useEffect(() => {
    if (!db && dbs.length) setDb(dbs[0]);
  }, [db, dbs]);
  const detail = useApi<DbDetail>(db && ov.data?.backups === 'on' ? `/api/db/${encodeURIComponent(db)}` : null, [gen]);
  const [mode, setMode] = useState<'backup' | 'time' | 'newest'>(backupId ? 'backup' : 'newest');
  const [pick, setPick] = useState<string>(backupId ?? '');
  const [when, setWhen] = useState('');
  const [into, setInto] = useState('');
  const [touched, setTouched] = useState(false);
  useEffect(() => {
    if (!touched && db) setInto(`${db}_restore_${stamp()}`);
  }, [db, touched]);
  const backups: Backup[] = detail.data?.backups ?? [];
  useEffect(() => {
    if (mode === 'backup' && !pick && backups.length) setPick(String(backups[0].id));
  }, [mode, pick, backups]);

  const chosen = backups.find((b) => String(b.id) === pick);
  const time = mode === 'backup' ? (chosen ? backupTime(chosen.taken_at) : '') : mode === 'time' ? fromInput(when) : '';
  const problems = useMemo(() => {
    const p: string[] = [];
    if (!into) p.push('name the NEW database');
    else if (into === db) p.push('--into must be a NEW database, never the source');
    else if (dbs.includes(into)) p.push(`database '${into}' already exists: pick a new name (the live database is never overwritten)`);
    if (mode === 'backup' && !chosen) p.push('pick a backup');
    if (mode === 'time' && !when) p.push('pick a time');
    return p;
  }, [into, db, dbs, mode, chosen, when]);

  const cmd = `pgbx db-restore --db ${shq(db || 'DB')} --into ${shq(into || 'NEWDB')}${time ? ` --time ${shq(time)}` : ''}${profileFlag}`;

  if (ov.error) return <ErrorCard error={ov.error} />;
  if (!ov.data) return <Loading />;
  if (ov.data.backups === 'off')
    return (
      <div className="card notice">
        <h2>Restore helper</h2>
        <p>Backups are off on this server (no pgbx extension), so there is nothing to restore here.</p>
        <p className="mute">
          To rebuild a database from dumps already in S3 onto this server, see <code>pgbx db-restore --from-s3</code> (pgbx help).
        </p>
      </div>
    );

  const restore = () =>
    confirm({
      title: `Restore ${db} into a NEW database ${into}?`,
      body: (
        <>
          <p>
            Creates the new database <b>{into}</b> from {mode === 'backup' && chosen ? <>backup #{chosen.id} ({ts(chosen.taken_at)})</> : time ? <>the newest backup at or before {time}</> : 'the newest backup'}.
          </p>
          <p className="mute">{db} itself is not touched. pgbx never restores over an existing database.</p>
        </>
      ),
      cli: cmd,
      confirm: 'Restore into new database',
      run: async () => {
        const r = await api<{ job_id: number }>('/api/action/restore', { db, into, time: time || undefined });
        go('overview');
        return r;
      },
    });

  return (
    <>
      <div className="card">
        <h1 className="h1">Restore helper</h1>
        <p className="mute">
          Pick a backup or a point in time. pgbx restores only into a <b>new</b> database; your live database is never written.
        </p>
        <div className="form">
          <label>
            <span>database</span>
            <select value={db} onChange={(e) => setDb(e.target.value)}>
              {dbs.map((d) => (
                <option key={d}>{d}</option>
              ))}
            </select>
          </label>
          <fieldset className="seg">
            <legend>restore from</legend>
            {(
              [
                ['newest', 'newest backup'],
                ['backup', 'a backup'],
                ['time', 'a time (UTC)'],
              ] as const
            ).map(([m, label]) => (
              <label key={m} className={mode === m ? 'on' : ''}>
                <input type="radio" name="mode" value={m} checked={mode === m} onChange={() => setMode(m)} />
                {label}
              </label>
            ))}
          </fieldset>
          {mode === 'backup' && (
            <label>
              <span>backup</span>
              <select value={pick} onChange={(e) => setPick(e.target.value)}>
                {backups.map((b) => (
                  <option key={b.id} value={b.id}>
                    #{b.id} · {ts(b.taken_at)} · {ago(b.taken_at)} · {b.size}
                  </option>
                ))}
              </select>
            </label>
          )}
          {mode === 'time' && (
            <label>
              <span>time (UTC): the newest backup at or before it</span>
              <input type="datetime-local" step={1} value={when} onChange={(e) => setWhen(e.target.value)} />
            </label>
          )}
          <label>
            <span>into NEW database</span>
            <input
              value={into}
              spellCheck={false}
              autoCapitalize="off"
              onChange={(e) => {
                setTouched(true);
                setInto(e.target.value.trim());
              }}
            />
          </label>
        </div>
        {detail.error && <p className="bad">{detail.error}</p>}
        {mode === 'backup' && detail.data && !backups.length && <p className="warn">{db} has no backups yet.</p>}
        {problems.map((p) => (
          <p key={p} className="warn small">
            {p}
          </p>
        ))}
      </div>
      <div className="card">
        <h2>The command</h2>
        <CommandLine cmd={cmd} />
        <p className="mute small">
          Add <code>--wait</code> to wait for it. It queues a restore job on the server; watch it in the job queue on the overview.
        </p>
        {session.actions.includes('restore') && (
          <button type="button" className="btn primary" disabled={problems.length > 0} onClick={restore}>
            Restore into new database…
          </button>
        )}
      </div>
    </>
  );
}
