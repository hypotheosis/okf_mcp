// Entry point of `just mcpjam-swarm`: provisions the okf-mcp side of a live
// MCPJam swarm, checks it, and tears it down (see README, "Live swarm").
//
//   npx tsx src/swarm-cli.ts <goalId> [--sessions N] [--keep] [--dry-run]
//
// Environment (never on the command line, never printed):
//   OKF_MCP_URL  the MCP endpoint, e.g. https://staging.example.com/mcp
//   OKF_MCP_KEY  an admin key reaching every catalog (setup and teardown only)
import { randomBytes } from "node:crypto";

import { CatalogAdmin } from "./admin.js";
import { OkfSession, type Target } from "./okf.js";
import { fixtureSeeds, KeyAdmin, runSwarm, type Hooks, type SessionInfo } from "./swarm.js";

/** Parsed command line. */
export interface Args {
    goalId: string;
    sessions: number;
    keep: boolean;
    dryRun: boolean;
}

/**
 * Parses the command line.
 *
 * @param argv - the arguments after the script name.
 * @returns the arguments.
 * @throws on a missing goal id, an unknown flag or a bad session count.
 */
export function parseArgs(argv: string[]): Args {
    const args: Args = { goalId: "", sessions: 1, keep: false, dryRun: false };
    for (let i = 0; i < argv.length; i++) {
        const arg = argv[i] as string;
        if (arg === "--keep") args.keep = true;
        else if (arg === "--dry-run") args.dryRun = true;
        else if (arg === "--sessions") {
            const value = Number(argv[++i]);
            // Each session is billed on MCPJam's side; 50 is a sanity cap.
            if (!Number.isInteger(value) || value < 1 || value > 50) {
                throw new Error("--sessions must be an integer from 1 to 50");
            }
            args.sessions = value;
        } else if (arg.startsWith("--")) throw new Error(`unknown flag ${arg}`);
        else if (args.goalId === "") args.goalId = arg;
        else throw new Error(`unexpected argument ${arg}`);
    }
    if (args.goalId === "") throw new Error("usage: mcpjam-swarm <goalId> [--sessions N] [--keep] [--dry-run]");
    return args;
}

function required(name: string): string {
    const value = process.env[name];
    if (value === undefined || value === "") throw new Error(`${name} is required`);
    return value;
}

// A Target for OkfSession: only `url` and `key` matter to `open`.
function targetFor(url: string, key: string): Target {
    return { mode: "remote", url, key, isolation: "catalog", catalogs: {}, runFolder: "" };
}

/** The real seeding and verification, over MCP. */
export function realHooks(url: string, adminKey: string, log: (line: string) => void): Hooks {
    return {
        log,
        async seed(catalog) {
            const session = await OkfSession.open(targetFor(url, adminKey), catalog);
            try {
                const changeset = await session.openChangeset();
                for (const seed of fixtureSeeds()) {
                    await session.call("create_concept", {
                        path: seed.target,
                        frontmatter: { type: seed.type, title: seed.title, description: seed.description },
                        body: seed.body,
                        index_summary: seed.description,
                        changeset,
                    });
                }
                await session.publish(changeset, "mcpjam swarm: seed fixtures");
            } finally {
                await session.close();
            }
        },
        // The key reaches its own catalog as its default, and no other.
        async verify(session: SessionInfo, secret: string, others: string[]) {
            const own = await OkfSession.open(targetFor(url, secret), session.catalog);
            try {
                const ntp = await own.attempt("get_concept", { path: "/infra/ntp.md" });
                if (!ntp.ok) throw new Error(`session ${session.index}: its key cannot read /infra/ntp.md`);
                const [other] = others;
                if (other !== undefined) {
                    const foreign = await own.attempt("get_concept", {
                        path: "/infra/ntp.md",
                        catalog: other,
                    });
                    if (foreign.ok) {
                        throw new Error(`session ${session.index}: its key can read ${other}`);
                    }
                }
            } finally {
                await own.close();
            }
        },
    };
}

/** Runs the CLI; returns the exit code. */
export async function main(argv: string[], log: (line: string) => void): Promise<number> {
    let args: Args;
    try {
        args = parseArgs(argv);
    } catch (error) {
        log(error instanceof Error ? error.message : String(error));
        return 2;
    }
    const prefix = `mcpjam-${Date.now().toString(36)}-${randomBytes(3).toString("hex")}`;
    const abort = new AbortController();
    const onSignal = (): void => abort.abort();
    process.once("SIGINT", onSignal);
    process.once("SIGTERM", onSignal);

    // A dry run needs no environment at all: it calls nothing.
    log(`goal ${args.goalId}, ${args.sessions} session(s), run ${prefix}`);
    try {
        const url = args.dryRun ? (process.env.OKF_MCP_URL ?? "") : required("OKF_MCP_URL");
        const adminKey = args.dryRun ? "" : required("OKF_MCP_KEY");
        const apiBase =
            process.env.OKF_MCP_API_URL ?? (url === "" ? "" : `${new URL(url).origin}/api/v1`);
        await runSwarm(
            { prefix, sessions: args.sessions, mcpUrl: url, keep: args.keep, dryRun: args.dryRun, signal: abort.signal },
            new CatalogAdmin(apiBase, adminKey, prefix),
            new KeyAdmin(apiBase, adminKey, prefix),
            realHooks(url, adminKey, log),
        );
        return 0;
    } catch (error) {
        log(`failed: ${error instanceof Error ? error.message : String(error)}`);
        return abort.signal.aborted ? 130 : 1;
    }
}

if (import.meta.url === `file://${process.argv[1]}`) {
    main(process.argv.slice(2), (line) => console.info(line)).then((code) => process.exit(code));
}
