// The live-swarm provisioner (src/swarm.ts): its orchestration against a fake
// okf-mcp REST API, and one real round trip against the suite's server. No
// MCPJam is involved: that side is not built (README, "Live swarm").
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

import { CatalogAdmin } from "../src/admin.js";
import { target } from "../src/harness.js";
import { fixtureSeeds, KeyAdmin, runSwarm, type Hooks, type Options } from "../src/swarm.js";
import { main, parseArgs, realHooks } from "../src/swarm-cli.js";

const PREFIX = "mcpjam-run1";
const API = "http://okf-swarm-test.invalid/api/v1";
const ADMIN_KEY = "ADMIN-SECRET-VALUE";

/** A fake of the REST calls the provisioner makes, recording each one. */
class FakeApi {
    calls: string[] = [];
    secrets: string[] = [];
    /** Fail the Nth request matching this "METHOD /path-prefix", 1-based. */
    failOn: { match: string; nth: number; reject?: boolean; afterStore?: boolean } | null = null;
    /** Replaces the answer of a key create (the key is stored first). */
    keyAnswer: ((name: string, secret: string) => Response) | null = null;
    /** Replaces the answer of a catalog create (the catalog is stored first). */
    catalogAnswer: ((name: string) => Response) | null = null;
    /** Catalogs that exist server-side (a create of one answers 409). */
    serverCatalogs = new Set<string>();
    /** Keys that exist server-side: name to id and revoked flag. */
    serverKeys = new Map<string, { id: string; revoked: boolean }>();
    /** Create the Nth key server-side, but lose the answer (the request rejects). */
    loseKeyAnswer: number | null = null;
    private seen = new Map<string, number>();
    private keyCount = 0;

    fetch = async (input: string | URL | Request, init?: RequestInit): Promise<Response> => {
        const url = String(input);
        const method = init?.method ?? "GET";
        const path = url.slice(API.length).replace(/\?.*$/, "");
        const label = `${method} ${path}`;
        this.calls.push(label);
        const headers = new Headers(init?.headers);
        if (headers.get("Authorization") !== `Bearer ${ADMIN_KEY}`) {
            return new Response("unauthorized", { status: 401 });
        }
        if (this.failOn !== null && label.startsWith(this.failOn.match)) {
            const count = (this.seen.get(this.failOn.match) ?? 0) + 1;
            this.seen.set(this.failOn.match, count);
            if (count === this.failOn.nth) {
                // afterStore: the server commits the create, then the answer is lost or a 500.
                if (this.failOn.afterStore === true) await this.handle(method, path, init, label);
                // A transport error: the request rejects instead of answering.
                if (this.failOn.reject === true) throw new TypeError("fetch failed");
                return new Response("boom", { status: 500 });
            }
        }
        return this.handle(method, path, init, label);
    };

    private async handle(
        method: string,
        path: string,
        init: RequestInit | undefined,
        label: string,
    ): Promise<Response> {
        if (label === "GET /me") return Response.json({ user: "alice" });
        if (method === "POST" && path === "/catalogs") {
            const { name } = JSON.parse(String(init?.body)) as { name: string };
            if (this.serverCatalogs.has(name)) return new Response("taken", { status: 409 });
            this.serverCatalogs.add(name);
            if (this.catalogAnswer !== null) return this.catalogAnswer(name);
            return Response.json({ name }, { status: 201 });
        }
        if (method === "DELETE" && path.startsWith("/catalogs/")) return new Response(null, { status: 204 });
        if (method === "POST" && path === "/keys") {
            const body = JSON.parse(String(init?.body)) as { name: string };
            const secret = `SESSION-SECRET-${++this.keyCount}`;
            this.secrets.push(secret);
            this.serverKeys.set(body.name, { id: `id-${body.name}`, revoked: false });
            if (this.keyCount === this.loseKeyAnswer) throw new TypeError("fetch failed");
            if (this.keyAnswer !== null) return this.keyAnswer(body.name, secret);
            return Response.json({ key: { id: `id-${body.name}`, name: body.name }, secret }, { status: 200 });
        }
        if (label === "GET /keys") {
            return Response.json({
                keys: [...this.serverKeys].map(([name, k]) => ({
                    id: k.id,
                    name,
                    revoked_at: k.revoked ? "t" : null,
                })),
            });
        }
        if (method === "POST" && path.endsWith("/revoke")) {
            for (const k of this.serverKeys.values()) {
                if (path === `/keys/${k.id}/revoke`) k.revoked = true;
            }
            return Response.json({});
        }
        return new Response("not found", { status: 404 });
    }
}

let api: FakeApi;
let lines: string[];
let events: string[];

function hooks(overrides: Partial<Hooks> = {}): Hooks {
    return {
        log: (line) => lines.push(line),
        seed: async (catalog) => {
            events.push(`seed ${catalog}`);
        },
        verify: async (session) => {
            events.push(`verify ${session.catalog}`);
        },
        ...overrides,
    };
}

function options(extra: Partial<Options> = {}): Options {
    return { prefix: PREFIX, sessions: 2, mcpUrl: "https://example.com/mcp", keep: false, dryRun: false, ...extra };
}

function clients(): { catalogs: CatalogAdmin; keys: KeyAdmin } {
    return {
        catalogs: new CatalogAdmin(API, ADMIN_KEY, PREFIX),
        keys: new KeyAdmin(API, ADMIN_KEY, PREFIX),
    };
}

beforeEach(() => {
    api = new FakeApi();
    lines = [];
    events = [];
    vi.stubGlobal("fetch", api.fetch);
});
afterEach(() => {
    vi.unstubAllGlobals();
});

/** The destructive calls, in order. */
const teardownCalls = (): string[] =>
    api.calls.filter((c) => c.startsWith("DELETE") || c.endsWith("/revoke"));

describe("setup and teardown order", () => {
    test("creates and seeds the catalogs, then the keys, verifies, then revokes keys before deleting catalogs", async () => {
        const { catalogs, keys } = clients();
        const sessions = await runSwarm(options(), catalogs, keys, hooks());
        expect(sessions.map((s) => s.catalog)).toEqual([`${PREFIX}-1`, `${PREFIX}-2`]);
        expect(api.calls).toEqual([
            "GET /me",
            "POST /catalogs",
            "POST /catalogs",
            "POST /keys",
            "POST /keys",
            `POST /keys/id-${PREFIX}-1/revoke`,
            `POST /keys/id-${PREFIX}-2/revoke`,
            `DELETE /catalogs/${PREFIX}-1`,
            `DELETE /catalogs/${PREFIX}-2`,
        ]);
        expect(events).toEqual([
            `seed ${PREFIX}-1`,
            `seed ${PREFIX}-2`,
            `verify ${PREFIX}-1`,
            `verify ${PREFIX}-2`,
        ]);
    });

    test("each key reaches only its own catalog, with it as the default", async () => {
        const bodies: Record<string, unknown>[] = [];
        const inner = api.fetch;
        vi.stubGlobal("fetch", async (input: string, init?: RequestInit) => {
            if (init?.method === "POST" && input.endsWith("/keys")) {
                bodies.push(JSON.parse(String(init.body)) as Record<string, unknown>);
            }
            return inner(input, init);
        });
        const { catalogs, keys } = clients();
        await runSwarm(options(), catalogs, keys, hooks());
        expect(bodies).toEqual([
            { name: `${PREFIX}-1`, access: "write", catalogs: [`${PREFIX}-1`], default_catalog: `${PREFIX}-1`, user: "alice" },
            { name: `${PREFIX}-2`, access: "write", catalogs: [`${PREFIX}-2`], default_catalog: `${PREFIX}-2`, user: "alice" },
        ]);
    });

    test("verify is told the other sessions' catalogs, never the admin key", async () => {
        const seen: { secret: string; others: string[] }[] = [];
        const { catalogs, keys } = clients();
        await runSwarm(options(), catalogs, keys, hooks({
            verify: async (_s, secret, others) => {
                seen.push({ secret, others });
            },
        }));
        expect(seen).toEqual([
            { secret: "SESSION-SECRET-1", others: [`${PREFIX}-2`] },
            { secret: "SESSION-SECRET-2", others: [`${PREFIX}-1`] },
        ]);
        expect(seen.some((s) => s.secret === ADMIN_KEY)).toBe(false);
    });
});

describe("a failure tears down what exists and stops", () => {
    const cases: { step: string; setup: () => Partial<Hooks> | void; expected: string[] }[] = [
        {
            step: "the second catalog create",
            setup: () => void (api.failOn = { match: "POST /catalogs", nth: 2 }),
            expected: [`DELETE /catalogs/${PREFIX}-1`],
        },
        {
            step: "a seed",
            setup: () => ({
                seed: async (catalog: string) => {
                    if (catalog.endsWith("-2")) throw new Error("seed failed");
                },
            }),
            expected: [`DELETE /catalogs/${PREFIX}-1`, `DELETE /catalogs/${PREFIX}-2`],
        },
        {
            step: "the second key create",
            setup: () => void (api.failOn = { match: "POST /keys", nth: 2 }),
            expected: [
                `POST /keys/id-${PREFIX}-1/revoke`,
                `DELETE /catalogs/${PREFIX}-1`,
                `DELETE /catalogs/${PREFIX}-2`,
            ],
        },
        {
            step: "verification",
            setup: () => ({
                verify: async () => {
                    throw new Error("verify failed");
                },
            }),
            expected: [
                `POST /keys/id-${PREFIX}-1/revoke`,
                `POST /keys/id-${PREFIX}-2/revoke`,
                `DELETE /catalogs/${PREFIX}-1`,
                `DELETE /catalogs/${PREFIX}-2`,
            ],
        },
    ];
    for (const { step, setup, expected } of cases) {
        test(`after ${step}`, async () => {
            const override = setup();
            const { catalogs, keys } = clients();
            await expect(
                runSwarm(options(), catalogs, keys, hooks(override ?? {})),
            ).rejects.toThrow();
            expect(teardownCalls()).toEqual(expected);
            // onReady (the launch) never ran: it is not even given the chance.
        });
    }

    test("onReady is never called after a setup failure", async () => {
        const onReady = vi.fn(async () => {});
        api.failOn = { match: "POST /keys", nth: 1 };
        const { catalogs, keys } = clients();
        await expect(runSwarm(options(), catalogs, keys, hooks({ onReady }))).rejects.toThrow();
        expect(onReady).not.toHaveBeenCalled();
    });

    test("a failure in onReady tears down too", async () => {
        const { catalogs, keys } = clients();
        await expect(
            runSwarm(options(), catalogs, keys, hooks({
                onReady: async () => {
                    throw new Error("launch failed");
                },
            })),
        ).rejects.toThrow(/launch failed/);
        expect(teardownCalls()).toHaveLength(4);
    });

    test("an abort (Ctrl-C) tears down and ends the run", async () => {
        const abort = new AbortController();
        const { catalogs, keys } = clients();
        await expect(
            runSwarm(options({ signal: abort.signal }), catalogs, keys, hooks({
                seed: async () => abort.abort(),
            })),
        ).rejects.toThrow(/interrupted/);
        expect(teardownCalls()).toEqual([`DELETE /catalogs/${PREFIX}-1`, `DELETE /catalogs/${PREFIX}-2`]);
        expect(api.calls).not.toContain("POST /keys");
    });

    test("a teardown that cannot delete reports what is left and fails the run", async () => {
        api.failOn = { match: `DELETE /catalogs/${PREFIX}-2`, nth: 1 };
        const { catalogs, keys } = clients();
        await expect(runSwarm(options(), catalogs, keys, hooks())).rejects.toThrow(/left 1 resources/);
        expect(lines.join("\n")).toMatch(new RegExp(`left behind: catalog ${PREFIX}-2`));
    });
});

describe("a transport error during cleanup", () => {
    test("a rejected first revoke does not stop the other revokes or any catalog delete, and is reported", async () => {
        api.failOn = { match: "POST /keys/", nth: 1, reject: true };
        const { catalogs, keys } = clients();
        await expect(runSwarm(options(), catalogs, keys, hooks())).rejects.toThrow(/left 1 resources/);
        const revokes = api.calls.filter((c) => c.endsWith("/revoke"));
        expect(revokes).toEqual([
            `POST /keys/id-${PREFIX}-1/revoke`,
            `POST /keys/id-${PREFIX}-2/revoke`,
        ]);
        expect(api.calls.filter((c) => c.startsWith("DELETE"))).toEqual([
            `DELETE /catalogs/${PREFIX}-1`,
            `DELETE /catalogs/${PREFIX}-2`,
        ]);
        expect(lines.join("\n")).toContain(`left behind: key ${PREFIX}-1 id-${PREFIX}-1 (TypeError: fetch failed)`);
    });

    test("a rejected catalog delete does not stop the other deletes, and is reported", async () => {
        api.failOn = { match: "DELETE /catalogs/", nth: 1, reject: true };
        const { catalogs, keys } = clients();
        await expect(runSwarm(options(), catalogs, keys, hooks())).rejects.toThrow(/left 1 resources/);
        expect(api.calls.filter((c) => c.startsWith("DELETE"))).toHaveLength(2);
        expect(lines.join("\n")).toContain(`left behind: catalog ${PREFIX}-1 (TypeError: fetch failed)`);
    });

    test("on a setup failure, a rejected revoke still leaves the catalogs deleted and names the key", async () => {
        api.failOn = { match: "POST /keys/", nth: 1, reject: true };
        const { catalogs, keys } = clients();
        await expect(
            runSwarm(options(), catalogs, keys, hooks({
                verify: async () => {
                    throw new Error("verify failed");
                },
            })),
        ).rejects.toThrow(/verify failed/);
        expect(api.calls.filter((c) => c.startsWith("DELETE"))).toHaveLength(2);
        expect(lines.join("\n")).toMatch(/left behind: key mcpjam-run1-1/);
    });

    test("a key whose create answer was lost is reported unresolved, never revoked by name", async () => {
        api.failOn = { match: "POST /keys", nth: 2, reject: true, afterStore: true };
        const { catalogs, keys } = clients();
        await expect(runSwarm(options(), catalogs, keys, hooks())).rejects.toThrow(/fetch failed/);
        // Key 1 (confirmed) is revoked; key 2 is not touched.
        expect(api.calls.filter((c) => c.endsWith("/revoke"))).toEqual([`POST /keys/id-${PREFIX}-1/revoke`]);
        expect(api.calls).not.toContain("GET /keys");
        expect(lines.join("\n")).toContain(`unresolved: key ${PREFIX}-2`);
        expect(lines.join("\n")).toContain("may or may not exist");
    });

    test("a key the server stored before answering 500 is reported unresolved, not revoked", async () => {
        api.failOn = { match: "POST /keys", nth: 1, afterStore: true };
        const { catalogs, keys } = clients();
        await expect(runSwarm(options(), catalogs, keys, hooks())).rejects.toThrow();
        expect(api.calls.filter((c) => c.endsWith("/revoke"))).toEqual([]);
        expect(lines.join("\n")).toContain(`unresolved: key ${PREFIX}-1 (HTTP 500`);
    });

    test("a catalog whose create answer was lost is reported unresolved, never deleted", async () => {
        api.failOn = { match: "POST /catalogs", nth: 2, reject: true, afterStore: true };
        const { catalogs, keys } = clients();
        await expect(runSwarm(options(), catalogs, keys, hooks())).rejects.toThrow(/fetch failed/);
        // Catalog 1 (confirmed) is deleted; catalog 2 exists server-side and is left alone.
        expect(api.calls.filter((c) => c.startsWith("DELETE"))).toEqual([`DELETE /catalogs/${PREFIX}-1`]);
        expect(lines.join("\n")).toContain(`unresolved: catalog ${PREFIX}-2`);
    });

    test("a catalog the server stored before answering 500 is reported unresolved, not deleted", async () => {
        api.failOn = { match: "POST /catalogs", nth: 1, afterStore: true };
        const { catalogs, keys } = clients();
        await expect(runSwarm(options(), catalogs, keys, hooks())).rejects.toThrow();
        expect(api.calls.filter((c) => c.startsWith("DELETE"))).toEqual([]);
        expect(lines.join("\n")).toContain(`unresolved: catalog ${PREFIX}-1 (HTTP 500`);
    });

    test("a catalog that already existed (409) is not tracked, and a lost 409 is never deleted", async () => {
        // Pre-existing, not ours; the 409 answer is lost on the wire.
        api.serverCatalogs.add(`${PREFIX}-1`);
        api.failOn = { match: "POST /catalogs", nth: 1, reject: true };
        const { catalogs, keys } = clients();
        await expect(runSwarm(options(), catalogs, keys, hooks())).rejects.toThrow(/fetch failed/);
        expect(api.calls.filter((c) => c.startsWith("DELETE"))).toEqual([]);
        expect(lines.join("\n")).toContain(`unresolved: catalog ${PREFIX}-1`);
    });

    test("a definitive 409 is not tracked at all", async () => {
        api.serverCatalogs.add(`${PREFIX}-1`);
        const { catalogs, keys } = clients();
        await expect(runSwarm(options(), catalogs, keys, hooks())).rejects.toThrow(/409/);
        expect(api.calls.filter((c) => c.startsWith("DELETE"))).toEqual([]);
        expect(lines.join("\n")).not.toContain("unresolved");
    });
});

describe("a create answer that is malformed or the wrong shape", () => {
    const LEAK = "LEAK-9f3a-secret-text";
    /** The three places a leak could surface. */
    const surfaces = async (
        keys: KeyAdmin,
        catalogs: CatalogAdmin,
        run: Promise<unknown>,
    ): Promise<string> => {
        const error = await run.catch((e: unknown) => e);
        return [String(error), lines.join("\n"), ...keys.unresolvedCreates(), ...catalogs.unresolvedCreates()].join("\n");
    };

    test("malformed JSON containing a secret never reaches the error, the logs or the unresolved reason", async () => {
        api.keyAnswer = () => new Response(`{"secret": "${LEAK}" oops`, { status: 200 });
        const { catalogs, keys } = clients();
        const out = await surfaces(keys, catalogs, runSwarm(options(), catalogs, keys, hooks()));
        expect(out).not.toContain(LEAK);
        expect(out).toContain("key create answer was not valid JSON");
        expect(out).toContain(`unresolved: key ${PREFIX}-1`);
        expect(api.calls.filter((c) => c.endsWith("/revoke"))).toEqual([]);
    });

    test("a catalog 201 with malformed JSON is unresolved, never deleted, and leaks nothing", async () => {
        api.catalogAnswer = () => new Response(`{"x": "${LEAK}" oops`, { status: 201 });
        const { catalogs, keys } = clients();
        const out = await surfaces(keys, catalogs, runSwarm(options(), catalogs, keys, hooks()));
        expect(out).not.toContain(LEAK);
        expect(out).toContain(`unresolved: catalog ${PREFIX}-1`);
        expect(api.calls.filter((c) => c.startsWith("DELETE"))).toEqual([]);
    });

    const shapes: [string, string][] = [
        ["{}", "{}"],
        ["a null key", '{"key":null}'],
        ["a missing id", `{"key":{"name":"${PREFIX}-1"},"secret":"${LEAK}"}`],
        ["an empty id", `{"key":{"id":"","name":"${PREFIX}-1"},"secret":"${LEAK}"}`],
        ["a missing secret", `{"key":{"id":"id-x","name":"${PREFIX}-1"}}`],
        ["a null secret", `{"key":{"id":"id-x","name":"${PREFIX}-1"},"secret":null}`],
        ["a missing name", `{"key":{"id":"id-x"},"secret":"${LEAK}"}`],
        ["a different name", `{"key":{"id":"id-x","name":"other"},"secret":"${LEAK}"}`],
        ["an array", "[]"],
    ];
    for (const [what, text] of shapes) {
        test(`a key 200 with ${what} is unresolved by name; nothing is revoked`, async () => {
            api.keyAnswer = () => new Response(text, { status: 200 });
            const { catalogs, keys } = clients();
            const out = await surfaces(keys, catalogs, runSwarm(options(), catalogs, keys, hooks()));
            expect(out).toContain(`unresolved: key ${PREFIX}-1`);
            expect(out).not.toContain(LEAK);
            expect(api.calls.filter((c) => c.endsWith("/revoke"))).toEqual([]);
            expect(api.calls.join("\n")).not.toContain("undefined");
        });
    }

    for (const [what, text] of [["{}", "{}"], ["the wrong name", '{"name":"other"}'], ["null", "null"]] as const) {
        test(`a catalog 201 with ${what} is unresolved; nothing is deleted`, async () => {
            api.catalogAnswer = () => new Response(text, { status: 201 });
            const { catalogs, keys } = clients();
            const out = await surfaces(keys, catalogs, runSwarm(options(), catalogs, keys, hooks()));
            expect(out).toContain(`unresolved: catalog ${PREFIX}-1`);
            expect(api.calls.filter((c) => c.startsWith("DELETE"))).toEqual([]);
        });
    }

    test("a key 201 with a usable body is accepted like a 200", async () => {
        api.keyAnswer = (name) =>
            Response.json({ key: { id: `id-${name}`, name }, secret: "s" }, { status: 201 });
        const { catalogs, keys } = clients();
        await runSwarm(options(), catalogs, keys, hooks());
        expect(keys.unresolvedCreates()).toEqual([]);
    });

    test("a /me answer of the wrong shape is refused without echoing the body", async () => {
        const inner = api.fetch;
        vi.stubGlobal("fetch", async (input: string, init?: RequestInit) =>
            String(input).endsWith("/me") ? new Response(`oops ${LEAK}`) : inner(input, init));
        const { catalogs } = clients();
        const error = await catalogs.owner().catch((e: Error) => e);
        expect(String(error)).not.toContain(LEAK);
        expect(String(error)).toContain("unusable answer");
    });
});

describe("--keep and --dry-run", () => {
    test("--keep deletes and revokes nothing and says what is left", async () => {
        const { catalogs, keys } = clients();
        await runSwarm(options({ keep: true }), catalogs, keys, hooks());
        expect(teardownCalls()).toEqual([]);
        const out = lines.join("\n");
        expect(out).toContain(`catalog ${PREFIX}-1`);
        expect(out).toContain(`id id-${PREFIX}-2`);
    });

    test("--keep still tears down after a failure", async () => {
        api.failOn = { match: "POST /keys", nth: 1 };
        const { catalogs, keys } = clients();
        await expect(runSwarm(options({ keep: true }), catalogs, keys, hooks())).rejects.toThrow();
        expect(teardownCalls()).toEqual([`DELETE /catalogs/${PREFIX}-1`, `DELETE /catalogs/${PREFIX}-2`]);
    });

    test("--dry-run makes no HTTP call at all, MCPJam or okf", async () => {
        const { catalogs, keys } = clients();
        const sessions = await runSwarm(options({ dryRun: true }), catalogs, keys, hooks());
        expect(sessions).toEqual([]);
        expect(api.calls).toEqual([]);
        expect(events).toEqual([]);
        expect(lines.join("\n")).toContain(`would create catalog ${PREFIX}-1`);
    });

    test("the CLI's dry run needs no environment and no network", async () => {
        const out: string[] = [];
        const code = await main(["goal-1", "--sessions", "3", "--dry-run"], (l) => out.push(l));
        expect(code).toBe(0);
        expect(api.calls).toEqual([]);
        expect(out.filter((l) => l.startsWith("would create catalog"))).toHaveLength(3);
    });
});

describe("the guard", () => {
    test("KeyAdmin refuses a key name outside the run's prefix, before any request", async () => {
        const keys = new KeyAdmin(API, ADMIN_KEY, PREFIX);
        await expect(keys.create("someone-elses", "alice", `${PREFIX}-1`)).rejects.toThrow(/refusing/);
        expect(await keys.revokeCreated()).toEqual([]);
        expect(api.calls).toEqual([]);
    });

    test("CatalogAdmin refuses foreign catalogs through the swarm's client too", async () => {
        const { catalogs } = clients();
        await expect(catalogs.create("production", "alice")).rejects.toThrow(/refusing production/);
        expect(await catalogs.deleteCreated()).toEqual([]);
        expect(teardownCalls()).toEqual([]);
    });

    test("the run only ever deletes and revokes what it created", async () => {
        const { catalogs, keys } = clients();
        await runSwarm(options({ sessions: 3 }), catalogs, keys, hooks());
        for (const call of teardownCalls()) expect(call).toContain(PREFIX);
        expect(teardownCalls()).toHaveLength(6);
    });
});

describe("secrets", () => {
    test("no admin or session secret appears in any output, on success, keep or failure", async () => {
        for (const run of ["ok", "keep", "fail"] as const) {
            api = new FakeApi();
            lines = [];
            vi.stubGlobal("fetch", api.fetch);
            if (run === "fail") api.failOn = { match: "POST /keys", nth: 2 };
            const { catalogs, keys } = clients();
            await runSwarm(options({ keep: run === "keep" }), catalogs, keys, hooks()).catch(
                (error: unknown) => lines.push(String(error)),
            );
            const out = lines.join("\n");
            expect(out).not.toContain(ADMIN_KEY);
            for (const secret of api.secrets) expect(out).not.toContain(secret);
            expect(out).not.toMatch(/SECRET/);
        }
    });

    test("a refused key create does not echo the response body", async () => {
        const keys = new KeyAdmin(API, ADMIN_KEY, PREFIX);
        vi.stubGlobal("fetch", async () => new Response("echoed SESSION-SECRET-X", { status: 500 }));
        const error = await keys.create(`${PREFIX}-1`, "alice", `${PREFIX}-1`).catch((e: Error) => e);
        expect(String(error)).not.toContain("SESSION-SECRET");
    });
});

describe("arguments and fixtures", () => {
    test("parseArgs", () => {
        expect(parseArgs(["g", "--sessions", "4", "--keep"])).toEqual({ goalId: "g", sessions: 4, keep: true, dryRun: false });
        expect(() => parseArgs([])).toThrow(/usage/);
        expect(() => parseArgs(["g", "--sessions", "0"])).toThrow(/1 to 50/);
        expect(() => parseArgs(["g", "--nope"])).toThrow(/unknown flag/);
    });

    test("the fixtures hold /infra/ntp once, and each goal's folder", () => {
        const targets = fixtureSeeds().map((s) => s.target);
        expect(targets.filter((t) => t === "infra/ntp")).toHaveLength(1);
        expect(targets).toContain("verify-and-stamp/readme");
        expect(targets).not.toContain("concurrency/readme");
        expect(new Set(targets).size).toBe(targets.length);
    });
});

// The real thing: provisioning against the suite's server. Needs the admin key
// that catalog isolation already requires.
describe("a real round trip", () => {
    test("provisions seeded catalogs and scoped keys, verifies them, and removes everything", async () => {
        vi.unstubAllGlobals();
        const t = target();
        if (t.isolation !== "catalog") return;
        const apiBase = process.env.OKF_MCP_API_URL ?? `${new URL(t.url).origin}/api/v1`;
        const prefix = `mcpjam-swarmtest-${Date.now().toString(36)}`;
        const out: string[] = [];
        const catalogs = new CatalogAdmin(apiBase, t.key, prefix);
        const keys = new KeyAdmin(apiBase, t.key, prefix);
        const sessions = await runSwarm(
            { prefix, sessions: 2, mcpUrl: t.url, keep: false, dryRun: false },
            catalogs,
            keys,
            realHooks(t.url, t.key, (l) => out.push(l)),
        );
        expect(sessions).toHaveLength(2);
        expect(out.join("\n")).not.toContain(t.key);
        // Gone: nothing left for the guard to delete or revoke.
        expect(await catalogs.deleteCreated()).toEqual([]);
        expect(keys.live).toEqual([]);
    });
});
