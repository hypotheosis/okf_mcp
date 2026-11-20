// Provisioning for a live MCPJam swarm: one seeded catalog and one catalog-scoped
// write key per session, and a teardown that always runs.
//
// This is the okf-mcp side only. MCPJam's preview API has no way to give an
// environment its own URL or bearer (see README, "Live swarm"), so the part
// that creates environments and launches the goal is a follow-up; `onReady`
// is where it plugs in.
//
// The admin key is used for setup and teardown only and is never handed out.
// Each session key reaches exactly one catalog (with it as its default). Key
// secrets live only in memory, in `KeyAdmin`; nothing here logs or returns one.
import { CatalogAdmin } from "./admin.js";
import { SCENARIOS, type Seed } from "./scenarios.js";

/** One provisioned session: what a human or a follow-up needs to wire it up. */
export interface SessionInfo {
    /** 1-based session number. */
    index: number;
    /** The session's temporary catalog. */
    catalog: string;
    /** The session key's name. */
    keyName: string;
    /** The session key's id (not its secret). */
    keyId: string;
    /** The okf-mcp MCP endpoint the key is for. */
    mcpUrl: string;
}

/** A key as the REST API answers a create: `{ key: { id }, secret }`. */
interface CreatedKey {
    id: string;
    secret: string;
}

/**
 * Creates and revokes this run's session keys, and nothing else. Like
 * `CatalogAdmin`, it refuses to touch a key it did not create: an admin key
 * can revoke any key on the server.
 */
export class KeyAdmin {
    /** Keys created and not yet revoked: id to name. */
    private readonly created = new Map<string, string>();
    /**
     * Names whose create request got no answer: the key may exist. The
     * teardown looks them up by name (`GET /keys`) and revokes what it finds.
     */
    private readonly pending = new Set<string>();
    /** Secrets by key id. Never exposed except through `secretOf`. */
    private readonly secrets = new Map<string, string>();

    /**
     * @param apiBase - the admin API's base, e.g. `https://host/api/v1`.
     * @param adminKey - the admin key, sent as a bearer token.
     * @param prefix - every key name this run creates starts with `<prefix>-`.
     */
    constructor(
        private readonly apiBase: string,
        private readonly adminKey: string,
        readonly prefix: string,
    ) {}

    /**
     * Creates a write key that reaches only `catalog`, with it as its default.
     *
     * @param name - the key's name; it must start with `<prefix>-`.
     * @param user - the user the key belongs to.
     * @param catalog - the one catalog it may reach.
     * @returns the key's id. The secret stays inside this object.
     * @throws when the name is foreign or the server refuses.
     */
    async create(name: string, user: string, catalog: string): Promise<string> {
        this.assertPrefixed(name);
        // Pending until an answer settles it: a lost answer leaves a key we
        // cannot name by id, so the teardown finds it by name.
        this.pending.add(name);
        const response = await fetch(`${this.apiBase}/keys`, {
            method: "POST",
            headers: {
                Authorization: `Bearer ${this.adminKey}`,
                "Content-Type": "application/json",
            },
            body: JSON.stringify({
                name,
                access: "write",
                catalogs: [catalog],
                default_catalog: catalog,
                user,
            }),
        });
        // Not `response.text()` in the error: a create answer holds a secret.
        if (response.status !== 200 && response.status !== 201) {
            // The server answered no: there is no key to look for.
            this.pending.delete(name);
            throw new Error(`POST /keys ${name}: ${response.status}`);
        }
        const body = (await response.json()) as { key: { id: string }; secret: string };
        this.pending.delete(name);
        const created: CreatedKey = { id: body.key.id, secret: body.secret };
        this.created.set(created.id, name);
        this.secrets.set(created.id, created.secret);
        return created.id;
    }

    /** The secret of a key this instance created; for the follow-up that hands it to MCPJam. */
    secretOf(id: string): string {
        const secret = this.secrets.get(id);
        if (secret === undefined) throw new Error(`no secret held for key ${id}`);
        return secret;
    }

    /**
     * Revokes every key this instance created.
     *
     * @returns the keys it could not revoke, each with the reason.
     */
    async revokeCreated(): Promise<string[]> {
        const kept: string[] = [];
        for (const [id, name] of [...this.created]) {
            this.assertOwned(id, name);
            // Per key: a transport error on one must not stop the rest.
            await this.revokeOne(id, name, kept);
        }
        await this.revokePending(kept);
        return kept;
    }

    private async revokeOne(id: string, name: string, kept: string[]): Promise<void> {
        try {
            const response = await fetch(`${this.apiBase}/keys/${encodeURIComponent(id)}/revoke`, {
                method: "POST",
                headers: { Authorization: `Bearer ${this.adminKey}` },
            });
            // 404: gone already, which is what the teardown wants.
            if (response.ok || response.status === 404) {
                this.created.delete(id);
                this.secrets.delete(id);
            } else {
                kept.push(`${name} ${id} (${response.status})`);
            }
        } catch (error) {
            kept.push(`${name} ${id} (${String(error)})`);
        }
    }

    // Keys whose create answer was lost: find them by exact name and revoke.
    private async revokePending(kept: string[]): Promise<void> {
        if (this.pending.size === 0) return;
        try {
            const response = await fetch(`${this.apiBase}/keys`, {
                headers: { Authorization: `Bearer ${this.adminKey}` },
            });
            if (!response.ok) throw new Error(`GET /keys: ${response.status}`);
            const { keys } = (await response.json()) as {
                keys: { id: string; name: string; revoked_at: string | null }[];
            };
            for (const key of keys) {
                if (!this.pending.has(key.name) || key.revoked_at !== null) continue;
                this.assertPrefixed(key.name);
                await this.revokeOne(key.id, key.name, kept);
            }
            // Looked up: whatever was there is revoked or reported.
            for (const name of [...this.pending]) this.pending.delete(name);
        } catch (error) {
            for (const name of this.pending) kept.push(`${name} (possible orphan: ${String(error)})`);
        }
    }

    /** The ids still held, for the `--keep` report. */
    get live(): string[] {
        return [...this.created.keys(), ...this.pending];
    }

    private assertOwned(id: string, name: string): void {
        this.assertPrefixed(name);
        if (this.created.get(id) !== name) {
            throw new Error(`refusing to revoke ${id}: this run did not create it`);
        }
    }

    private assertPrefixed(name: string): void {
        if (!name.startsWith(`${this.prefix}-`)) {
            throw new Error(`refusing ${name}: not one of this run's keys (${this.prefix}-*)`);
        }
    }
}

/**
 * The fixtures every session catalog starts with: the seeds of every swarm goal
 * (so `/infra/ntp.md` exists for "Verify and stamp a fact"), at the catalog's
 * root, once each. A goal that only needs its folder gets `/<slug>/readme`.
 *
 * @returns seeds with their final path (no leading slash, no `.md`).
 */
export function fixtureSeeds(): (Seed & { target: string })[] {
    const seen = new Set<string>();
    const out: (Seed & { target: string })[] = [];
    for (const scenario of Object.values(SCENARIOS)) {
        // The concurrency scenario is not a swarm goal.
        if (scenario.slug === "concurrency") continue;
        for (const seed of scenario.seeds) {
            const target = seed.path === "readme" ? `${scenario.slug}/readme` : seed.path;
            if (seen.has(target)) continue;
            seen.add(target);
            out.push({ ...seed, target });
        }
    }
    return out;
}

/** The side effects `provision` needs besides the two admin clients. */
export interface Hooks {
    /** Publishes the fixtures into `catalog`. */
    seed(catalog: string): Promise<void>;
    /** Checks a session's key: it reads its own catalog and no other. Throws if not. */
    verify(session: SessionInfo, secret: string, others: string[]): Promise<void>;
    /** Where the MCPJam automation will plug in: runs with every session ready. */
    onReady?(sessions: SessionInfo[], signal: AbortSignal | undefined): Promise<void>;
    /** One line of output. Callers never pass a secret. */
    log(line: string): void;
}

/** What to provision. */
export interface Options {
    /** Names every catalog and key: `<prefix>-<i>`. */
    prefix: string;
    sessions: number;
    mcpUrl: string;
    keep: boolean;
    dryRun: boolean;
    /** Aborted by Ctrl-C: the run stops at the next step and tears down. */
    signal?: AbortSignal;
}

/**
 * Sets the sessions up, then tears them down (unless `keep`), always.
 *
 * Order: all catalogs, then seed each, then one key per catalog, then verify
 * each key. A failure at any step tears down what exists and rethrows;
 * nothing is launched. `keep` skips only the normal-path teardown.
 *
 * @param options - what to provision.
 * @param catalogs - the guarded catalog client (its prefix must be `options.prefix`).
 * @param keys - the guarded key client (same prefix).
 * @param hooks - seeding, verification and output.
 * @returns the sessions that were provisioned (empty for a dry run).
 * @throws the first setup, verify or teardown failure.
 */
export async function runSwarm(
    options: Options,
    catalogs: CatalogAdmin,
    keys: KeyAdmin,
    hooks: Hooks,
): Promise<SessionInfo[]> {
    const names = Array.from({ length: options.sessions }, (_, i) => ({
        index: i + 1,
        catalog: `${options.prefix}-${i + 1}`,
        keyName: `${options.prefix}-${i + 1}`,
    }));
    if (options.dryRun) {
        hooks.log(`dry run: nothing is created, no API is called`);
        for (const { catalog, keyName } of names) {
            hooks.log(`would create catalog ${catalog}, seed it, and key ${keyName} reaching only it`);
        }
        hooks.log(`would then verify each key, and tear down: keys first, then catalogs`);
        return [];
    }

    let sessions: SessionInfo[] = [];
    const teardown = async (): Promise<string[]> => {
        // Keys first: no key outlives its catalog. A failure of one step never
        // skips the other: both always run, and every leftover is named.
        const left = await keys.revokeCreated().catch((e: unknown) => [`(revoking failed: ${String(e)})`]);
        const gone = await catalogs.deleteCreated().catch((e: unknown) => [`(deleting failed: ${String(e)})`]);
        return [...left.map((k) => `key ${k}`), ...gone.map((c) => `catalog ${c}`)];
    };

    // Checked between steps: a signal ends the run through the same teardown.
    const checkpoint = (): void => {
        if (options.signal?.aborted === true) throw new Error("interrupted");
    };

    try {
        const owner = await catalogs.owner();
        for (const { catalog } of names) {
            checkpoint();
            const refusal = await catalogs.create(catalog, owner);
            if (refusal !== null) throw new Error(`the admin key may not create catalogs`);
        }
        for (const { catalog } of names) {
            checkpoint();
            await hooks.seed(catalog);
        }
        sessions = [];
        for (const { index, catalog, keyName } of names) {
            checkpoint();
            const keyId = await keys.create(keyName, owner, catalog);
            sessions.push({ index, catalog, keyName, keyId, mcpUrl: options.mcpUrl });
        }
        for (const session of sessions) {
            checkpoint();
            const others = sessions.filter((s) => s !== session).map((s) => s.catalog);
            await hooks.verify(session, keys.secretOf(session.keyId), others);
        }
        for (const s of sessions) {
            hooks.log(`session ${s.index}: catalog ${s.catalog}, key ${s.keyName} (id ${s.keyId}), ${s.mcpUrl}`);
        }
        await hooks.onReady?.(sessions, options.signal);
        checkpoint();
    } catch (error) {
        const left = await teardown().catch((e: unknown) => [`teardown failed: ${String(e)}`]);
        if (left.length > 0) hooks.log(`left behind: ${left.join(", ")}`);
        throw error;
    }

    if (options.keep) {
        hooks.log(`--keep: left ${sessions.length} catalogs and keys in place`);
        for (const s of sessions) {
            hooks.log(`  catalog ${s.catalog}, key ${s.keyName} (id ${s.keyId})`);
        }
        hooks.log("  the key secrets were not saved; mint a new key per catalog to use them");
        hooks.log("  remove with `okf-mcp key revoke <id>` then `okf-mcp catalog delete <name> --force`");
        return sessions;
    }
    const left = await teardown();
    if (left.length > 0) {
        hooks.log(`left behind: ${left.join(", ")}`);
        throw new Error(`teardown left ${left.length} resources behind`);
    }
    hooks.log(`torn down: ${sessions.length} keys revoked, ${sessions.length} catalogs deleted`);
    return sessions;
}
