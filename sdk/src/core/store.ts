import Database from "better-sqlite3";
import { mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { defaultRun, type LoopEvent, type Run } from "./events.js";

export function loopdDir(): string {
  const dir = join(homedir(), ".loopd");
  mkdirSync(dir, { recursive: true });
  return dir;
}

interface RunRow {
  runId: string;
  label: string;
  agent: string;
  cwd: string;
  status: string;
  pid: number | null;
  iteration: number;
  costUsd: number;
  startedAt: number;
  endedAt: number | null;
  lastEventAt: number;
  flags: string;
  killRequested: number;
  owned: number;
}

function rowToRun(r: RunRow): Run {
  return {
    ...r,
    status: r.status as Run["status"],
    flags: JSON.parse(r.flags || "[]"),
    killRequested: !!r.killRequested,
    owned: !!r.owned,
  };
}

/**
 * SQLite-backed store shared across processes (WAL mode), so `loop run`,
 * `loop daemon`, and `loop dash` can all read/write the same state without
 * a custom IPC layer.
 */
export class Store {
  db: Database.Database;

  constructor(path?: string) {
    this.db = new Database(path ?? join(loopdDir(), "loopd.db"));
    this.db.pragma("journal_mode = WAL");
    this.db.pragma("busy_timeout = 5000");
    this.migrate();
  }

  private migrate() {
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS runs (
        runId        TEXT PRIMARY KEY,
        label        TEXT NOT NULL,
        agent        TEXT NOT NULL,
        cwd          TEXT NOT NULL,
        status       TEXT NOT NULL,
        pid          INTEGER,
        iteration    INTEGER NOT NULL DEFAULT 0,
        costUsd      REAL NOT NULL DEFAULT 0,
        startedAt    INTEGER NOT NULL,
        endedAt      INTEGER,
        lastEventAt  INTEGER NOT NULL,
        flags        TEXT NOT NULL DEFAULT '[]',
        killRequested INTEGER NOT NULL DEFAULT 0,
        owned        INTEGER NOT NULL DEFAULT 0
      );
      CREATE TABLE IF NOT EXISTS events (
        id        INTEGER PRIMARY KEY AUTOINCREMENT,
        runId     TEXT NOT NULL,
        source    TEXT NOT NULL,
        type      TEXT NOT NULL,
        tool      TEXT,
        iteration INTEGER,
        tokensIn  INTEGER,
        tokensOut INTEGER,
        costUsd   REAL,
        text      TEXT,
        ts        INTEGER NOT NULL
      );
      CREATE INDEX IF NOT EXISTS idx_events_run ON events(runId, id);
    `);
  }

  insertEvent(e: LoopEvent): void {
    this.db
      .prepare(
        `INSERT INTO events (runId, source, type, tool, iteration, tokensIn, tokensOut, costUsd, text, ts)
         VALUES (@runId, @source, @type, @tool, @iteration, @tokensIn, @tokensOut, @costUsd, @text, @ts)`,
      )
      .run({
        tool: null,
        iteration: null,
        tokensIn: null,
        tokensOut: null,
        costUsd: null,
        text: null,
        ...e,
      });
  }

  getRun(runId: string): Run | undefined {
    const row = this.db
      .prepare(`SELECT * FROM runs WHERE runId = ?`)
      .get(runId) as RunRow | undefined;
    return row ? rowToRun(row) : undefined;
  }

  listRuns(): Run[] {
    const rows = this.db
      .prepare(`SELECT * FROM runs ORDER BY startedAt DESC`)
      .all() as RunRow[];
    return rows.map(rowToRun);
  }

  upsertRun(patch: Partial<Run> & { runId: string }): Run {
    const merged: Run = { ...(this.getRun(patch.runId) ?? defaultRun(patch.runId)), ...patch };
    this.db
      .prepare(
        `INSERT INTO runs (runId,label,agent,cwd,status,pid,iteration,costUsd,startedAt,endedAt,lastEventAt,flags,killRequested,owned)
         VALUES (@runId,@label,@agent,@cwd,@status,@pid,@iteration,@costUsd,@startedAt,@endedAt,@lastEventAt,@flags,@killRequested,@owned)
         ON CONFLICT(runId) DO UPDATE SET
           label=@label, agent=@agent, cwd=@cwd, status=@status, pid=@pid,
           iteration=@iteration, costUsd=@costUsd, startedAt=@startedAt, endedAt=@endedAt,
           lastEventAt=@lastEventAt, flags=@flags, killRequested=@killRequested, owned=@owned`,
      )
      .run({
        ...merged,
        flags: JSON.stringify(merged.flags),
        killRequested: merged.killRequested ? 1 : 0,
        owned: merged.owned ? 1 : 0,
      });
    return merged;
  }

  eventsForRun(runId: string, limit = 200): LoopEvent[] {
    return this.db
      .prepare(`SELECT * FROM events WHERE runId = ? ORDER BY id DESC LIMIT ?`)
      .all(runId, limit) as unknown as LoopEvent[];
  }

  requestKill(runId: string): void {
    this.db.prepare(`UPDATE runs SET killRequested = 1 WHERE runId = ?`).run(runId);
  }

  close(): void {
    this.db.close();
  }
}
