// Temporary catalogs through the REST admin API (`/api/v1/catalogs`).
//
// Creating and deleting a catalog needs an admin key that reaches every
// catalog (`key create ... --access admin --all`) or a provisioner key. Such a
// key can delete ANY catalog on the server, so this client refuses to delete
// anything it did not create itself in this run: see `assertOwned`.

/** Why a catalog could not be created. */
export type CreateRefusal = "forbidden";

/** Creates and deletes this run's temporary catalogs, and nothing else. */
export class CatalogAdmin {
    /** The catalogs this instance created and has not deleted yet. */
    private readonly created = new Set<string>();
    /** Creates with no definitive answer: name to the reason. Never deleted. */
    private readonly unresolved = new Map<string, string>();

    /**
     * @param apiBase - the admin API's base, e.g. `https://host/api/v1`.
     * @param key - the API key, sent as a bearer token.
     * @param prefix - every name this run creates starts with `<prefix>-`;
     *   a name without it is never created or deleted.
     */
    constructor(
        private readonly apiBase: string,
        private readonly key: string,
        readonly prefix: string,
    ) {}

    /** The key's user: its personal team (same name) owns the catalogs. */
    async owner(): Promise<string> {
        const response = await this.request("GET", "/me");
        if (!response.ok) throw await failure("GET /me", response);
        const body = await readJson(response);
        const user = isRecord(body) ? body.user : undefined;
        if (typeof user !== "string" || user === "") throw new Error("GET /me: unusable answer");
        return user;
    }

    /**
     * Creates a catalog holding only its starter files.
     *
     * @param name - the catalog's name; it must start with `<prefix>-`.
     * @param owner - the owning team.
     * @returns `null` when created, `"forbidden"` when the key may not create catalogs.
     * @throws for any other failure (a taken name, a server error).
     */
    async create(name: string, owner: string): Promise<CreateRefusal | null> {
        this.assertPrefixed(name);
        let response: Response;
        try {
            response = await this.request("POST", "/catalogs", {
                name,
                owner,
                description: "Temporary catalog of an mcpjam scenario run; deleted when the run ends.",
            });
        } catch (error) {
            // No answer: the catalog may or may not exist. Not ours to delete.
            this.unresolved.set(name, String(error));
            throw error;
        }
        if (response.status === 201) {
            // Only a 201 whose body names this catalog makes it this run's own.
            // The body is parsed as `unknown` and its parser error is dropped.
            const body = await readJson(response);
            if (!isRecord(body) || body.name !== name) {
                this.unresolved.set(name, "catalog create answer was not usable");
                throw new Error(`POST /catalogs ${name}: unusable answer`);
            }
            this.created.add(name);
            return null;
        }
        // A 5xx proves nothing: the server may have created it first.
        if (response.status >= 500) this.unresolved.set(name, `HTTP ${response.status}`);
        // Any other refusal (403, 409, 422) is definitive: not created, not tracked.
        if (response.status === 403) return "forbidden";
        throw await failure(`POST /catalogs ${name}`, response);
    }

    /**
     * Catalogs whose create got no usable answer: they may or may not exist.
     * They are never deleted (that would be a guess); a human must check.
     *
     * @returns each name with why it is unresolved.
     */
    unresolvedCreates(): string[] {
        return [...this.unresolved].map(([name, why]) => `${name} (${why}; may or may not exist)`);
    }

    /**
     * Deletes every catalog this instance created, with its history and any
     * open changesets (`force=true`).
     *
     * @returns the names it could not delete, each with the reason.
     */
    async deleteCreated(): Promise<string[]> {
        const kept: string[] = [];
        for (const name of [...this.created]) {
            this.assertOwned(name);
            // Per catalog: a transport error on one must not stop the rest.
            try {
                const response = await this.request(
                    "DELETE",
                    `/catalogs/${encodeURIComponent(name)}?force=true`,
                );
                // 404: already gone, which is what the teardown wants.
                if (response.ok || response.status === 404) {
                    this.created.delete(name);
                } else {
                    kept.push(`${name} (${response.status} ${await response.text()})`);
                }
            } catch (error) {
                kept.push(`${name} (${String(error)})`);
            }
        }
        return kept;
    }

    // The guard: only a name this run made AND created may be deleted.
    private assertOwned(name: string): void {
        this.assertPrefixed(name);
        if (!this.created.has(name)) {
            throw new Error(`refusing to delete ${name}: this run did not create it`);
        }
    }

    private assertPrefixed(name: string): void {
        if (!name.startsWith(`${this.prefix}-`)) {
            throw new Error(`refusing ${name}: not one of this run's catalogs (${this.prefix}-*)`);
        }
    }

    private request(method: string, path: string, body?: unknown): Promise<Response> {
        return fetch(`${this.apiBase}${path}`, {
            method,
            headers: {
                Authorization: `Bearer ${this.key}`,
                ...(body === undefined ? {} : { "Content-Type": "application/json" }),
            },
            ...(body === undefined ? {} : { body: JSON.stringify(body) }),
        });
    }
}

/** A JSON object (not null, not an array). */
export function isRecord(value: unknown): value is Record<string, unknown> {
    return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Reads a response body as JSON.
 *
 * @returns the parsed value, or `undefined` when the body is not valid JSON.
 *   The parser's own error is never kept or rethrown: Node's message can quote
 *   part of the body, and a create answer may hold a secret.
 */
export async function readJson(response: Response): Promise<unknown> {
    try {
        return (await response.json()) as unknown;
    } catch {
        return undefined;
    }
}

async function failure(what: string, response: Response): Promise<Error> {
    return new Error(`${what}: ${response.status} ${await response.text()}`);
}
