// A thin, deterministic client for okf-mcp over MCP, built on the MCPJam SDK's
// MCPClientManager. No LLM is involved: every call is a direct `tools/call`.
//
// Each OkfSession is its own MCP connection (its own `Mcp-Session-Id`), so the
// concurrency scenarios can open several and race them against one catalog,
// the way concurrent swarm sessions do.
import { randomUUID } from "node:crypto";

import { MCPClientManager, assertCallToolResult } from "@mcpjam/sdk";

/** Where the suite runs: what the global setup provides to every test file. */
export interface Target {
    /** "local" (a server this run started) or "remote" (OKF_MCP_URL, e.g. staging). */
    mode: "local" | "remote";
    /** The server's MCP endpoint, e.g. `http://127.0.0.1:8080/mcp`. */
    url: string;
    /** An API key with write access to every catalog in `catalogs`. */
    key: string;
    /**
     * "catalog": each scenario has a temporary catalog of its own, created and
     * deleted by this run. "folder": every scenario shares one existing
     * catalog (OKF_MCP_CATALOG), each in its own folder.
     */
    isolation: "catalog" | "folder";
    /** The catalog of each scenario, by slug. */
    catalogs: Record<string, string>;
    /** This run's top folder, e.g. `swarm-m3k9x2-ab12`: every path is under it. */
    runFolder: string;
}

/** A tool's error answer: okf-mcp's `{ error, message, ... }` with `isError`. */
export interface ToolError {
    error: string;
    message: string;
    [key: string]: unknown;
}

/** A tool's answer either way, for calls whose outcome is the thing under test. */
export type Outcome =
    | { ok: true; data: Record<string, unknown> }
    | { ok: false; error: ToolError };

/** A tool call that failed when the scenario needed it to succeed (or the reverse). */
export class UnexpectedOutcome extends Error {
    constructor(tool: string, outcome: Outcome) {
        const detail = outcome.ok ? JSON.stringify(outcome.data) : JSON.stringify(outcome.error);
        super(`${tool}: expected the other outcome, got ${detail}`);
    }
}

/** One MCP connection to okf-mcp. */
export class OkfSession {
    private constructor(
        private readonly manager: MCPClientManager,
        private readonly serverId: string,
        readonly target: Target,
        /** The catalog this session's helpers read and write. */
        readonly catalog: string,
    ) {}

    /**
     * Opens a new MCP session against the target.
     *
     * @param target - where to connect, from `inject("target")`.
     * @param catalog - the catalog the helpers use (`catalog` in every call).
     * @returns the connected session; `close()` it when done.
     */
    static async open(target: Target, catalog: string): Promise<OkfSession> {
        const manager = new MCPClientManager();
        const serverId = `okf-${randomUUID()}`;
        await manager.connectToServer(serverId, {
            url: target.url,
            requestInit: { headers: { Authorization: `Bearer ${target.key}` } },
        });
        return new OkfSession(manager, serverId, target, catalog);
    }

    /** Ends the MCP session. */
    async close(): Promise<void> {
        await this.manager.disconnectServer(this.serverId);
    }

    /** The description of every tool the server lists, by tool name. */
    async toolDescriptions(): Promise<Record<string, string>> {
        const { tools } = await this.manager.listTools(this.serverId);
        return Object.fromEntries(tools.map((tool) => [tool.name, tool.description ?? ""]));
    }

    /**
     * Calls a tool and returns its outcome without judging it.
     *
     * @param tool - the tool's name.
     * @param args - its arguments.
     * @returns `{ ok: true, data }` with the structured result, or `{ ok: false, error }`.
     */
    async attempt(tool: string, args: Record<string, unknown> = {}): Promise<Outcome> {
        const result = assertCallToolResult(
            await this.manager.executeTool(this.serverId, tool, args),
            tool,
        );
        const data = (result.structuredContent ?? parseText(result.content)) as Record<
            string,
            unknown
        >;
        if (result.isError) {
            return { ok: false, error: data as ToolError };
        }
        return { ok: true, data };
    }

    /**
     * Calls a tool that must succeed.
     *
     * @returns its structured result.
     * @throws UnexpectedOutcome when the tool answers with an error.
     */
    async call<T = Record<string, unknown>>(
        tool: string,
        args: Record<string, unknown> = {},
    ): Promise<T> {
        const outcome = await this.attempt(tool, args);
        if (!outcome.ok) throw new UnexpectedOutcome(tool, outcome);
        return outcome.data as T;
    }

    /**
     * Calls a tool that must fail.
     *
     * @returns its error answer.
     * @throws UnexpectedOutcome when the tool succeeds.
     */
    async fail(tool: string, args: Record<string, unknown> = {}): Promise<ToolError> {
        const outcome = await this.attempt(tool, args);
        if (outcome.ok) throw new UnexpectedOutcome(tool, outcome);
        return outcome.error;
    }

    // The calls almost every scenario makes, with the catalog filled in.

    /** Opens a changeset in this session's catalog and returns its handle. */
    async openChangeset(): Promise<string> {
        const { changeset } = await this.call<{ changeset: string }>("open_changeset", {
            catalog: this.catalog,
        });
        return changeset;
    }

    /** Publishes a changeset that must publish cleanly. */
    async publish(changeset: string, message: string): Promise<PublishResult> {
        return this.call<PublishResult>("publish", { changeset, message });
    }

    /** Reads a published concept (or, with `changeset`, through its drafts). */
    async concept(path: string, extra: Record<string, unknown> = {}): Promise<Concept> {
        return this.call<Concept>("get_concept", {
            path,
            catalog: this.catalog,
            ...extra,
        });
    }

    /** Reads an index.md or log.md as raw text. */
    async document(path: string, changeset?: string): Promise<string> {
        const { text } = await this.call<{ text: string }>("read_document", {
            path,
            catalog: this.catalog,
            ...(changeset === undefined ? {} : { changeset }),
        });
        return text;
    }

    /** Searches this session's catalog; returns the hit paths under `folder` only. */
    async searchUnder(folder: string, query: string): Promise<string[]> {
        const { hits } = await this.call<{ hits: { path: string }[] }>("search", {
            query,
            catalogs: [this.catalog],
            limit: 200,
        });
        return hits.map((hit) => hit.path).filter((path) => path.startsWith(`${folder}/`));
    }
}

/** What `publish` answers. */
export interface PublishResult {
    published: boolean;
    revisions: { path: string; revision: number; op: string }[];
    warnings?: unknown[];
}

/** What `get_concept` answers (the parts the scenarios read). */
export interface Concept {
    path: string;
    title: string | null;
    frontmatter: Record<string, unknown>;
    body: string;
    raw?: string;
}

/** A `sources` entry as the server stores it. */
export interface SourceEntry {
    id: string;
    resource: string;
    title?: string;
    author?: string;
}

/** A stamp as the server writes it (`generated`, `verified[]`, `reviewed`). */
export interface Stamp {
    by: string;
    at: string;
    outcome?: string;
    note?: string;
}

// A result with no structuredContent carries its JSON as the first text block.
function parseText(content: { type: string; text?: string }[]): unknown {
    const block = content.find((item) => item.type === "text");
    return block?.text === undefined ? {} : JSON.parse(block.text);
}
