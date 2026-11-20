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
        const { user } = (await response.json()) as { user: string };
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
        // Recorded before the request: if the answer is lost (a transport
        // error) the catalog may exist, and the teardown must still try it.
        this.created.add(name);
        const response = await this.request("POST", "/catalogs", {
            name,
            owner,
            description: "Temporary catalog of an mcpjam scenario run; deleted when the run ends.",
        });
        // Any answer settles it: only 201 means this run made the catalog (a
        // 409 is somebody else's, and must never be deleted).
        if (response.status !== 201) this.created.delete(name);
        if (response.status === 403) return "forbidden";
        if (response.status !== 201) throw await failure(`POST /catalogs ${name}`, response);
        return null;
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

async function failure(what: string, response: Response): Promise<Error> {
    return new Error(`${what}: ${response.status} ${await response.text()}`);
}
